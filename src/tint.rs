//! Peacock-style chrome tint for connection windows.
//!
//! The tint is drawn by Colonnade's own window chrome (title bar, status bar,
//! sidebar edge, tab strip underline); Kit's global theme is never touched.

// allow: SIZE_OK - tests live inline with the module, as the plan requires.

use crate::color::{self, Rgb};
use crate::workspace::tabs::WindowKey;
use gpui_kit::{App, Hsla};

/// Colors for the tinted window chrome of one connection.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChromeTint {
    pub title_bar_bg: Hsla,
    pub title_bar_fg: Hsla,
    pub title_bar_inactive_bg: Hsla,
    pub title_bar_inactive_fg: Hsla,
    pub status_bar_bg: Hsla,
    pub status_bar_fg: Hsla,
    pub status_bar_hover_bg: Hsla,
    pub accent: Hsla,
}

fn dimmed(color: Hsla) -> Hsla {
    Hsla {
        a: color::INACTIVE_ALPHA,
        ..color
    }
}

impl ChromeTint {
    /// Derives every chrome color from a connection color.
    #[must_use]
    pub fn from_color(color: Rgb) -> Self {
        let bg = color.to_hsla();
        let fg = color::readable_foreground(color).to_hsla();
        Self {
            title_bar_bg: bg,
            title_bar_fg: fg,
            title_bar_inactive_bg: dimmed(bg),
            title_bar_inactive_fg: dimmed(fg),
            status_bar_bg: bg,
            status_bar_fg: fg,
            status_bar_hover_bg: color::hover_variant(color).to_hsla(),
            accent: bg,
        }
    }
}

/// The tint of a window: `Some` only for connection windows whose connection
/// still exists; main and launcher windows stay untinted.
#[must_use]
pub fn chrome_tint(window_key: WindowKey, cx: &App) -> Option<ChromeTint> {
    match window_key {
        WindowKey::Main => None,
        WindowKey::Connection(id) => cx
            .global::<crate::catalog::store::GlobalConnectionStore>()
            .0
            .read(cx)
            .profile(id)
            .map(|profile| ChromeTint::from_color(profile.color.effective())),
    }
}

/// Title bar `(background, foreground)` for the window activation state.
#[must_use]
pub const fn title_bar_colors(tint: &ChromeTint, active: bool) -> (Hsla, Hsla) {
    if active {
        (tint.title_bar_bg, tint.title_bar_fg)
    } else {
        (tint.title_bar_inactive_bg, tint.title_bar_inactive_fg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{
        model::{ConnectionId, ConnectionProfile, Driver, Endpoint, HexColor, PasswordStorage},
        ops::DropPosition,
        store::GlobalConnectionStore,
    };
    use crate::db::model::TlsMode;
    use crate::workspace::view::WorkspaceView;
    use gpui_kit::component::TitleBar;
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{AnyWindowHandle, AppContext as _, Entity, TestAppContext};

    const LIGHT: Rgb = Rgb {
        r: 0xF0,
        g: 0xE0,
        b: 0x60,
    };
    const DARK: Rgb = Rgb {
        r: 0x20,
        g: 0x30,
        b: 0x70,
    };

    fn alpha_u8(color: Hsla) -> u8 {
        let a = (color.a * 255.0).round();
        // Alpha is within 0..=255 here.
        format!("{a}").parse().unwrap()
    }

    #[test]
    fn from_color_light_background_gets_dark_foreground_and_darker_hover() {
        // Given: a light connection color.
        // When: deriving the tint.
        let tint = ChromeTint::from_color(LIGHT);
        // Then: background is the color, foreground the dark Peacock one, hover darker.
        assert_eq!(Rgb::from_hsla(tint.title_bar_bg), LIGHT);
        assert_eq!(Rgb::from_hsla(tint.accent), LIGHT);
        assert_eq!(
            Rgb::from_hsla(tint.title_bar_fg),
            color::PEACOCK_DARK_FOREGROUND
        );
        assert!(
            color::relative_luminance(Rgb::from_hsla(tint.status_bar_hover_bg))
                < color::relative_luminance(LIGHT)
        );
    }

    #[test]
    fn from_color_dark_background_gets_light_foreground_and_lighter_hover() {
        // Given: a dark connection color.
        // When: deriving the tint.
        let tint = ChromeTint::from_color(DARK);
        // Then: foreground is the light Peacock one and hover is lighter.
        assert_eq!(
            Rgb::from_hsla(tint.status_bar_fg),
            color::PEACOCK_LIGHT_FOREGROUND
        );
        assert!(
            color::relative_luminance(Rgb::from_hsla(tint.status_bar_hover_bg))
                > color::relative_luminance(DARK)
        );
    }

    #[test]
    fn inactive_title_bar_colors_have_peacock_alpha() {
        // Given: any tint.
        let tint = ChromeTint::from_color(DARK);
        // Then: active colors are opaque and inactive ones carry alpha 153/255.
        assert_eq!(alpha_u8(tint.title_bar_bg), 255);
        assert_eq!(alpha_u8(tint.title_bar_inactive_bg), 153);
        assert_eq!(alpha_u8(tint.title_bar_inactive_fg), 153);
    }

    #[test]
    fn generated_colors_keep_foreground_contrast_at_least_4_5() {
        // Given: generated connection colors for many seeds and the presets.
        let generated = (0..200).map(|n| color::generated_color(&format!("postgres://h{n}:5432")));
        for bg in generated.chain(color::connection_presets()) {
            // When: deriving the tint.
            let tint = ChromeTint::from_color(bg);
            // Then: text on title and status bars is legible.
            assert!(color::contrast_ratio(bg, Rgb::from_hsla(tint.title_bar_fg)) >= 4.5);
            assert!(color::contrast_ratio(bg, Rgb::from_hsla(tint.status_bar_fg)) >= 4.5);
        }
    }

    #[test]
    fn title_bar_colors_pick_the_pair_for_the_activation_state() {
        // Given: a tint.
        let tint = ChromeTint::from_color(DARK);
        // Then: active and inactive states select different pairs.
        assert_eq!(
            title_bar_colors(&tint, true),
            (tint.title_bar_bg, tint.title_bar_fg)
        );
        assert_eq!(
            title_bar_colors(&tint, false),
            (tint.title_bar_inactive_bg, tint.title_bar_inactive_fg)
        );
    }

    struct Fx {
        _dir: tempfile::TempDir,
        connection: ConnectionId,
    }

    fn setup(cx: &mut TestAppContext) -> Fx {
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| crate::app::init_for_tests(cx, dir.path()));
        let connection = cx.update(|cx| {
            let store = cx.global::<GlobalConnectionStore>().0.clone();
            store
                .update(cx, |s, cx| {
                    s.add_connection(
                        cx,
                        ConnectionProfile::new(
                            "Fixture",
                            Driver::Postgres,
                            Endpoint::Server {
                                host: "db.test".into(),
                                port: 5432,
                                user: "ada".into(),
                                database: None,
                                tls: TlsMode::default(),
                            },
                            PasswordStorage::None,
                        ),
                        DropPosition::RootEnd,
                    )
                })
                .unwrap()
        });
        Fx {
            _dir: dir,
            connection,
        }
    }

    type Open = (AnyWindowHandle, Entity<WorkspaceView>);

    fn open(cx: &mut TestAppContext, key: WindowKey) -> Open {
        cx.update(|cx| {
            let mut view = None;
            let (handle, _) =
                gpui_kit::open_window(TitleBar::window_options(), cx, |window, cx| {
                    let entity = cx.new(|cx| WorkspaceView::new(key, window, cx));
                    view = Some(entity.clone());
                    entity
                })
                .unwrap();
            (handle, view.unwrap())
        })
    }

    fn recolor(cx: &mut TestAppContext, id: ConnectionId, color: Rgb) {
        cx.update(|cx| {
            let store = cx.global::<GlobalConnectionStore>().0.clone();
            store.update(cx, |s, cx| {
                s.set_custom_color(cx, id, Some(HexColor::new(color)))
                    .unwrap();
            });
        });
    }

    fn sidebar_accent(cx: &mut TestAppContext, (window, view): &Open) -> Option<Hsla> {
        cx.update_window(*window, |_, window, cx| {
            window.render_frame(cx);
            view.read(cx).sidebar_accent(cx)
        })
        .unwrap()
    }

    #[gpui_kit::test]
    fn chrome_tint_is_none_for_main_and_some_for_a_connection(cx: &mut TestAppContext) {
        // Given: a saved connection.
        let fx = setup(cx);
        cx.update(|cx| {
            // When/Then: only the connection window is tinted.
            assert!(chrome_tint(WindowKey::Main, cx).is_none());
            assert!(chrome_tint(WindowKey::Connection(fx.connection), cx).is_some());
            assert!(chrome_tint(WindowKey::Connection(ConnectionId::new()), cx).is_none());
        });
    }

    #[gpui_kit::test]
    fn recoloring_a_connection_retints_its_open_window(cx: &mut TestAppContext) {
        // Given: an open connection window and an untinted main window.
        let fx = setup(cx);
        let connection_window = open(cx, WindowKey::Connection(fx.connection));
        let main_window = open(cx, WindowKey::Main);
        assert!(sidebar_accent(cx, &main_window).is_none());
        let before = sidebar_accent(cx, &connection_window).unwrap();
        // When: the connection's custom color changes.
        recolor(cx, fx.connection, DARK);
        // Then: the window's tint and sidebar accent follow; main stays untinted.
        let tint = cx.update(|cx| chrome_tint(WindowKey::Connection(fx.connection), cx).unwrap());
        assert_eq!(Rgb::from_hsla(tint.accent), DARK);
        let after = sidebar_accent(cx, &connection_window).unwrap();
        assert_ne!(before, after);
        assert_eq!(Rgb::from_hsla(after), DARK);
        assert!(sidebar_accent(cx, &main_window).is_none());
    }

    #[gpui_kit::test]
    fn status_model_carries_tint_only_for_connection_windows(cx: &mut TestAppContext) {
        // Given: a recolored connection.
        let fx = setup(cx);
        recolor(cx, fx.connection, LIGHT);
        cx.update(|cx| {
            // When: projecting the status of each window kind.
            let tinted = crate::workspace::status_bar::status_model(
                WindowKey::Connection(fx.connection),
                cx,
            );
            let plain = crate::workspace::status_bar::status_model(WindowKey::Main, cx);
            // Then: the connection window's status uses the tint colors.
            let tint = tinted.tint.unwrap();
            assert_eq!(Rgb::from_hsla(tint.status_bar_bg), LIGHT);
            assert_eq!(
                Rgb::from_hsla(tint.status_bar_fg),
                color::PEACOCK_DARK_FOREGROUND
            );
            assert!(plain.tint.is_none());
        });
    }
}
