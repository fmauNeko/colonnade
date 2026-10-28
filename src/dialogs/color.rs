//! Color picker dialog for connection colors.

use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::color_picker::{ColorPicker, ColorPickerEvent, ColorPickerState};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::{WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::{App, Entity, Window};

use crate::catalog::model::{ConnectionId, HexColor};
use crate::catalog::store::{ConnectionStore, GlobalConnectionStore};
use crate::color::{Rgb, connection_presets};

fn store(cx: &App) -> Entity<ConnectionStore> {
    cx.global::<GlobalConnectionStore>().0.clone()
}

fn set_custom(id: ConnectionId, color: Option<HexColor>, cx: &mut App) {
    // A failed save keeps the in-memory color, which is what the dialog shows.
    drop(store(cx).update(cx, |store, cx| store.set_custom_color(cx, id, color)));
}

/// Creates the picker state, applying every pick to the store at once.
fn open_picker(
    id: ConnectionId,
    window: &mut Window,
    cx: &mut App,
) -> Option<Entity<ColorPickerState>> {
    let effective = store(cx).read(cx).profile(id)?.color.effective();
    let picker = cx.new(|cx| ColorPickerState::new(window, cx).default_value(effective.to_hsla()));
    cx.subscribe(&picker, move |_, event: &ColorPickerEvent, cx| {
        let ColorPickerEvent::Change(color) = event;
        set_custom(
            id,
            color.map(|hsla| HexColor::new(Rgb::from_hsla(hsla))),
            cx,
        );
    })
    .detach();
    Some(picker)
}

/// Opens the color dialog of connection `id`: picks apply live, "Reset"
/// returns to the generated color and Cancel restores the previous value.
pub fn open(id: ConnectionId, window: &mut Window, cx: &mut App) {
    let Some(previous) = store(cx).read(cx).profile(id).map(|p| p.color.custom) else {
        return;
    };
    let Some(picker) = open_picker(id, window, cx) else {
        return;
    };
    let accepted = Rc::new(Cell::new(false));
    window.open_dialog(cx, move |dialog, _, _| {
        let presets = connection_presets().map(Rgb::to_hsla).to_vec();
        let reset = {
            let picker = picker.clone();
            Button::new("color-reset")
                .label("Reset")
                .on_click(move |_, window, cx| {
                    drop(store(cx).update(cx, |store, cx| store.reset_color(cx, id)));
                    let generated = store(cx).read(cx).profile(id).map(|p| p.color.generated);
                    if let Some(generated) = generated {
                        picker.update(cx, |state, cx| {
                            state.set_value(generated.rgb().to_hsla(), window, cx);
                        });
                    }
                })
        };
        let cancel = Button::new("dialog-cancel")
            .label("Cancel")
            .on_click(move |_, window, cx| {
                set_custom(id, previous, cx);
                window.close_dialog(cx);
            });
        let ok = {
            let accepted = accepted.clone();
            Button::new("dialog-ok")
                .label("OK")
                .primary()
                .on_click(move |_, window, cx| {
                    accepted.set(true);
                    window.close_dialog(cx);
                })
        };
        let restore = accepted.clone();
        dialog
            .title("Connection color")
            .child(
                v_flex().gap_3().child(
                    h_flex()
                        .gap_2()
                        .child(ColorPicker::new(&picker).featured_colors(presets))
                        .child(reset),
                ),
            )
            .footer(DialogFooter::new().child(cancel).child(ok))
            .on_close(move |_, _, cx| {
                if !restore.get() {
                    set_custom(id, previous, cx);
                }
            })
    });
}

#[cfg(test)]
mod tests {
    use gpui_kit::component::color_picker::ColorPickerState;
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{AnyWindowHandle, AppContext as _, TestAppContext};

    use super::super::confirm::harness::{dialog_closed, dialog_open, open_window};
    use super::{open, open_picker};
    use crate::catalog::model::{
        ConnectionId, ConnectionProfile, Driver, Endpoint, HexColor, PasswordStorage,
    };
    use crate::catalog::ops::DropPosition;
    use crate::catalog::store::{ConnectionStore, GlobalConnectionStore, init};
    use crate::color::Rgb;
    use crate::db::model::TlsMode;
    use crate::paths::ConfigPaths;

    fn setup(cx: &mut TestAppContext, dir: &tempfile::TempDir) -> (AnyWindowHandle, ConnectionId) {
        let handle = open_window(cx);
        let paths = ConfigPaths::at(dir.path().to_path_buf());
        let profile = ConnectionProfile::new(
            "c",
            Driver::Postgres,
            Endpoint::Server {
                host: "db.test".into(),
                port: 5432,
                user: "u".into(),
                database: None,
                tls: TlsMode::default(),
            },
            PasswordStorage::Ask,
        );
        let id = profile.id;
        cx.update(|cx| {
            init(cx, &paths);
            cx.global::<GlobalConnectionStore>()
                .0
                .clone()
                .update(cx, |store, cx| {
                    store.add_connection(cx, profile, DropPosition::RootEnd)
                })
        })
        .expect("add");
        (handle, id)
    }

    fn custom(cx: &mut TestAppContext, id: ConnectionId) -> Option<HexColor> {
        cx.update(|cx| {
            cx.global::<GlobalConnectionStore>()
                .0
                .read(cx)
                .profile(id)
                .expect("profile")
                .color
                .custom
        })
    }

    fn store(cx: &mut TestAppContext) -> gpui_kit::Entity<ConnectionStore> {
        cx.update(|cx| cx.global::<GlobalConnectionStore>().0.clone())
    }

    #[gpui_kit::test]
    async fn picking_applies_live_and_cancel_restores(cx: &mut TestAppContext) {
        // Given: a connection without custom color and the open color dialog.
        let dir = tempfile::tempdir().expect("tempdir");
        let (handle, id) = setup(cx, &dir);
        cx.update_window(handle, |_, window, cx| open(id, window, cx))
            .expect("window");
        dialog_open(cx, handle).await;
        // The test drives its own picker bound to the same store.
        let picker: gpui_kit::Entity<ColorPickerState> = cx
            .update_window(handle, |_, window, cx| {
                open_picker(id, window, cx).expect("profile exists")
            })
            .expect("window");
        // When: a color is picked.
        let pick = Rgb {
            r: 200,
            g: 20,
            b: 30,
        };
        cx.update_window(handle, |_, window, cx| {
            picker.update(cx, |state, cx| {
                state.select_color(pick.to_hsla(), window, cx);
            });
        })
        .expect("window");
        cx.run_until_parked();
        // Then: it is applied live; Cancel restores the previous (none).
        assert_eq!(custom(cx, id), Some(HexColor::new(pick)));
        cx.update_window(handle, |_, window, cx| {
            window.within("dialog").click("dialog-cancel", cx);
        })
        .expect("window");
        cx.run_until_parked();
        assert_eq!(custom(cx, id), None);
        dialog_closed(cx, handle).await;
    }

    #[gpui_kit::test]
    async fn reset_clears_and_ok_keeps_the_pick(cx: &mut TestAppContext) {
        // Given: a connection with a custom color and the open dialog.
        let dir = tempfile::tempdir().expect("tempdir");
        let (handle, id) = setup(cx, &dir);
        let previous = HexColor::new(Rgb { r: 1, g: 2, b: 3 });
        let store = store(cx);
        cx.update(|cx| store.update(cx, |s, cx| s.set_custom_color(cx, id, Some(previous))))
            .expect("set");
        cx.update_window(handle, |_, window, cx| open(id, window, cx))
            .expect("window");
        dialog_open(cx, handle).await;
        // When: resetting, then confirming.
        cx.update_window(handle, |_, window, cx| {
            window.within("dialog").click("color-reset", cx);
        })
        .expect("window");
        cx.run_until_parked();
        let after_reset = custom(cx, id);
        cx.update_window(handle, |_, window, cx| {
            window.within("dialog").click("dialog-ok", cx);
        })
        .expect("window");
        cx.run_until_parked();
        // Then: reset cleared it and OK left it cleared.
        assert_eq!(after_reset, None);
        assert_eq!(custom(cx, id), None);
        dialog_closed(cx, handle).await;
    }
}
