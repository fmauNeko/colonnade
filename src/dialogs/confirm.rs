//! Confirmation dialog for destructive actions.

use std::cell::RefCell;
use std::rc::Rc;

use futures::channel::oneshot;
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::prelude::*;
use gpui_kit::{App, SharedString, Window};

use crate::prompts::{ConfirmChoice, ConfirmRequest, PromptHooks, install_default};

/// The one-shot answer channel of an open dialog.
type Reply = Rc<RefCell<Option<oneshot::Sender<ConfirmChoice>>>>;

/// Sends `choice` unless the dialog was already answered; `true` when sent.
fn answer(reply: &Reply, choice: ConfirmChoice) -> bool {
    let pending = reply.borrow_mut().take();
    pending.is_some_and(|sender| {
        // A dropped receiver only means nobody waits for the answer.
        let _ = sender.send(choice);
        true
    })
}

fn choice_button(button: Button, reply: &Reply, choice: ConfirmChoice) -> Button {
    let reply = reply.clone();
    button.on_click(move |_, window, cx| {
        if answer(&reply, choice) {
            window.close_dialog(cx);
        }
    })
}

/// Opens the confirmation dialog for `req`; the receiver resolves with the
/// user's choice (`Cancel` when dismissed, e.g. with Escape).
pub fn open(
    window: &mut Window,
    cx: &mut App,
    req: ConfirmRequest,
) -> oneshot::Receiver<ConfirmChoice> {
    let (sender, receiver) = oneshot::channel();
    let reply: Reply = Rc::new(RefCell::new(Some(sender)));
    window.open_dialog(cx, move |dialog, _, _| {
        let cancel = choice_button(
            Button::new("confirm-cancel").label("Cancel"),
            &reply,
            ConfirmChoice::Cancel,
        );
        let confirm = Button::new("confirm-ok").label(req.confirm_label.clone());
        let confirm = if req.destructive {
            confirm.danger()
        } else {
            confirm.primary()
        };
        let confirm = choice_button(confirm, &reply, ConfirmChoice::Confirm);
        let alternatives = req.alternatives.iter().enumerate().map(|(index, label)| {
            let id = SharedString::from(format!("confirm-alt-{index}"));
            choice_button(
                Button::new(id).label(label.clone()),
                &reply,
                ConfirmChoice::Alternative(index),
            )
        });
        let dismissed = reply.clone();
        dialog
            .title(req.title.clone())
            .child(req.message.clone())
            .footer(
                DialogFooter::new()
                    .child(cancel)
                    .children(alternatives)
                    .child(confirm),
            )
            .on_close(move |_, _, _| {
                answer(&dismissed, ConfirmChoice::Cancel);
            })
    });
    receiver
}

/// Makes [`open`] the application's confirmation hook, keeping the other hooks.
pub fn install(cx: &mut App) {
    if !cx.has_global::<PromptHooks>() {
        install_default(cx);
    }
    let mut hooks = cx.global::<PromptHooks>().clone();
    hooks.confirm = Rc::new(open);
    cx.set_global(hooks);
}

#[cfg(test)]
pub(crate) mod harness {
    //! Headless window with the Kit `Root`, shared by the dialog tests.

    use std::time::Duration;

    use gpui_kit::AppContext as _;
    use gpui_kit::test::{TestAppContextExt as _, TestWindowExt as _};
    use gpui_kit::{
        AnyWindowHandle, Bounds, Context, IntoElement, Point, Render, TestAppContext, Window,
        WindowBounds, WindowOptions, div, px, size,
    };

    struct Blank;

    impl Render for Blank {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    /// Initializes Kit and opens a 900×900 window whose `Root` hosts dialogs.
    pub(crate) fn open_window(cx: &mut TestAppContext) -> AnyWindowHandle {
        cx.update(|cx| {
            gpui_kit::init(cx);
            // Entrance animations run on the wall clock: settle them at once.
            cx.set_reduce_motion(true);
        });
        let bounds = Bounds::new(Point::default(), size(px(900.), px(900.)));
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            ..Default::default()
        };
        let (handle, _) = cx
            .update(|cx| gpui_kit::open_window(options, cx, |_, cx| cx.new(|_| Blank)))
            .expect("the window opens");
        handle
    }

    /// Waits until a dialog is on screen.
    pub(crate) async fn dialog_open(cx: &mut TestAppContext, handle: AnyWindowHandle) {
        cx.wait_for(handle, Duration::from_secs(2), |window, _| {
            window.try_find("dialog").is_some()
        })
        .await;
    }

    /// Waits until no dialog is on screen.
    pub(crate) async fn dialog_closed(cx: &mut TestAppContext, handle: AnyWindowHandle) {
        cx.wait_for(handle, Duration::from_secs(2), |window, _| {
            window.try_find("dialog").is_none()
        })
        .await;
    }
}

#[cfg(test)]
mod tests {
    use futures::channel::oneshot::Receiver;
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{AnyWindowHandle, AppContext as _, TestAppContext};

    use super::harness::{dialog_closed, dialog_open, open_window};
    use super::{install, open};
    use crate::prompts::{ConfirmChoice, ConfirmRequest, confirm};

    fn request(destructive: bool) -> ConfirmRequest {
        ConfirmRequest {
            title: "Delete".into(),
            message: "Really?".into(),
            confirm_label: "Delete".into(),
            destructive,
            alternatives: vec!["Keep contents".into(), "Archive".into()],
        }
    }

    async fn ask(
        cx: &mut TestAppContext,
        handle: AnyWindowHandle,
        req: ConfirmRequest,
    ) -> Receiver<ConfirmChoice> {
        let receiver = cx
            .update_window(handle, |_, window, cx| confirm(window, cx, req))
            .expect("window");
        dialog_open(cx, handle).await;
        receiver
    }

    #[gpui_kit::test]
    async fn confirm_button_resolves_confirm(cx: &mut TestAppContext) {
        // Given: the real confirm hook and an open confirmation.
        let handle = open_window(cx);
        cx.update(install);
        let mut receiver = ask(cx, handle, request(true)).await;
        // When: the confirm button is clicked.
        cx.update_window(handle, |_, window, cx| {
            window.within("dialog").click("confirm-ok", cx);
        })
        .expect("window");
        // Then: the answer is Confirm and the dialog closes.
        assert_eq!(
            receiver.try_recv().expect("open"),
            Some(ConfirmChoice::Confirm)
        );
        dialog_closed(cx, handle).await;
    }

    #[gpui_kit::test]
    async fn alternative_button_resolves_its_index(cx: &mut TestAppContext) {
        // Given: a confirmation with two alternatives.
        let handle = open_window(cx);
        cx.update(install);
        let mut receiver = ask(cx, handle, request(false)).await;
        // When: the second alternative is clicked.
        cx.update_window(handle, |_, window, cx| {
            window.within("dialog").click("confirm-alt-1", cx);
        })
        .expect("window");
        // Then: the answer carries index 1.
        assert_eq!(
            receiver.try_recv().expect("open"),
            Some(ConfirmChoice::Alternative(1))
        );
        dialog_closed(cx, handle).await;
    }

    #[gpui_kit::test]
    async fn escape_and_cancel_resolve_cancel(cx: &mut TestAppContext) {
        // Given: the real confirm hook.
        let handle = open_window(cx);
        cx.update(install);
        // When: Escape is pressed on the first dialog, Cancel clicked on the second.
        let mut first = ask(cx, handle, request(true)).await;
        cx.update_window(handle, |_, window, cx| window.press("escape", cx))
            .expect("window");
        dialog_closed(cx, handle).await;
        let mut second = ask(cx, handle, request(true)).await;
        cx.update_window(handle, |_, window, cx| {
            window.within("dialog").click("confirm-cancel", cx);
        })
        .expect("window");
        // Then: both answer Cancel.
        assert_eq!(first.try_recv().expect("open"), Some(ConfirmChoice::Cancel));
        assert_eq!(
            second.try_recv().expect("open"),
            Some(ConfirmChoice::Cancel)
        );
    }

    #[gpui_kit::test]
    async fn open_is_usable_without_the_installed_hook(cx: &mut TestAppContext) {
        // Given: a window and no installed hooks.
        let handle = open_window(cx);
        // When: opening the dialog directly.
        let mut receiver = cx
            .update_window(handle, |_, window, cx| open(window, cx, request(false)))
            .expect("window");
        dialog_open(cx, handle).await;
        cx.update_window(handle, |_, window, cx| {
            window.within("dialog").click("confirm-ok", cx);
        })
        .expect("window");
        // Then: it resolves like the hook.
        assert_eq!(
            receiver.try_recv().expect("open"),
            Some(ConfirmChoice::Confirm)
        );
    }
}
