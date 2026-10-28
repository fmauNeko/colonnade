//! Password dialog: prompt for credentials.

use std::cell::Cell;
use std::rc::Rc;

use futures::channel::oneshot;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{WindowExt as _, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::{App, Context, Entity, IntoElement, Render, SharedString, Subscription, Window};

use crate::db::model::Secret;
use crate::prompts::{PasswordAnswer, PasswordRequest, PromptHooks, install_default};

type Reply = oneshot::Sender<Option<PasswordAnswer>>;

/// State and body of one open password prompt.
struct PasswordForm {
    input: Entity<InputState>,
    offer_keychain: bool,
    save_to_keychain: bool,
    reply: Option<Reply>,
    _enter: Subscription,
}

impl PasswordForm {
    /// Answers once; `true` when this call delivered the answer.
    fn finish(&mut self, answer: Option<PasswordAnswer>) -> bool {
        self.reply.take().is_some_and(|reply| {
            // A dropped receiver only means nobody waits for the answer.
            drop(reply.send(answer));
            true
        })
    }

    /// Answers with the typed password; `true` when this call delivered it.
    fn accept(&mut self, cx: &App) -> bool {
        let answer = PasswordAnswer {
            password: Secret::new(self.input.read(cx).value().to_string()),
            save_to_keychain: self.offer_keychain && self.save_to_keychain,
        };
        self.finish(Some(answer))
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.accept(cx) {
            window.close_dialog(cx);
        }
    }

    fn cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.finish(None) {
            window.close_dialog(cx);
        }
    }
}

impl Render for PasswordForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .gap_3()
            .child(Input::new(&self.input).id("password-input"))
            .when(self.offer_keychain, |this| {
                this.child(
                    Checkbox::new("password-save-keychain")
                        .label("Save to the keychain")
                        .checked(self.save_to_keychain)
                        .on_change(cx.listener(|this, checked, _, cx| {
                            this.save_to_keychain = *checked;
                            cx.notify();
                        })),
                )
            })
    }
}

/// Opens the password dialog; the receiver resolves with the answer, or `None`
/// when cancelled.
pub fn open(
    window: &mut Window,
    cx: &mut App,
    req: &PasswordRequest,
) -> oneshot::Receiver<Option<PasswordAnswer>> {
    let (sender, receiver) = oneshot::channel();
    let input = cx.new(|cx| InputState::new(window, cx).masked(true));
    let form = cx.new(|cx| PasswordForm {
        input: input.clone(),
        offer_keychain: req.offer_keychain,
        save_to_keychain: false,
        reply: Some(sender),
        _enter: cx.subscribe_in(
            &input,
            window,
            |this: &mut PasswordForm, _, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.submit(window, cx);
                }
            },
        ),
    });
    let title = SharedString::from(format!("Password for {}", req.connection_name));
    let initial_focus = Cell::new(true);
    window.open_dialog(cx, move |dialog, window, cx| {
        if initial_focus.replace(false) {
            input.update(cx, |input, cx| input.focus(window, cx));
        }
        let ok = {
            let form = form.clone();
            Button::new("dialog-ok")
                .label("OK")
                .primary()
                .on_click(move |_, window, cx| form.update(cx, |form, cx| form.submit(window, cx)))
        };
        let cancel = {
            let form = form.clone();
            Button::new("dialog-cancel")
                .label("Cancel")
                .on_click(move |_, window, cx| form.update(cx, |form, cx| form.cancel(window, cx)))
        };
        let dismissed = form.clone();
        let confirmed = form.clone();
        dialog
            .title(title.clone())
            .on_ok(move |_, _, cx| {
                confirmed.update(cx, |form, cx| form.accept(cx));
                true
            })
            .child(form.clone())
            .footer(DialogFooter::new().child(cancel).child(ok))
            .on_close(move |_, _, cx| {
                dismissed.update(cx, |form, _| {
                    form.finish(None);
                });
            })
    });
    receiver
}

/// Makes [`open`] the application's password hook, keeping the other hooks.
pub fn install(cx: &mut App) {
    if !cx.has_global::<PromptHooks>() {
        install_default(cx);
    }
    let mut hooks = cx.global::<PromptHooks>().clone();
    hooks.password = Rc::new(|window, cx, req| open(window, cx, &req));
    cx.set_global(hooks);
}

#[cfg(test)]
mod tests {
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{AppContext as _, TestAppContext};

    use super::super::confirm::harness::{dialog_closed, dialog_open, open_window};
    use super::install;
    use crate::catalog::model::ConnectionId;
    use crate::db::model::Secret;
    use crate::prompts::{PasswordAnswer, PasswordRequest, prompt_password};

    fn request(offer_keychain: bool) -> PasswordRequest {
        PasswordRequest {
            connection: ConnectionId::new(),
            connection_name: "prod".into(),
            offer_keychain,
        }
    }

    #[gpui_kit::test]
    async fn typed_password_and_checkbox_resolve(cx: &mut TestAppContext) {
        // Given: the real password hook and an open prompt offering the keychain.
        let handle = open_window(cx);
        cx.update(install);
        let mut receiver = cx
            .update_window(handle, |_, window, cx| {
                prompt_password(window, cx, request(true))
            })
            .expect("window");
        dialog_open(cx, handle).await;
        // When: typing a password, ticking the box and pressing OK.
        cx.update_window(handle, |_, window, cx| {
            window.within("dialog").click("password-input", cx);
            window.within("dialog").input("hunter2", cx);
            window.within("dialog").click("password-save-keychain", cx);
            window.within("dialog").click("dialog-ok", cx);
        })
        .expect("window");
        // Then: the answer holds both and the dialog closes.
        assert_eq!(
            receiver.try_recv().expect("open"),
            Some(Some(PasswordAnswer {
                password: Secret::new("hunter2".into()),
                save_to_keychain: true,
            }))
        );
        dialog_closed(cx, handle).await;
    }

    #[gpui_kit::test]
    async fn enter_submits_and_checkbox_is_absent_when_not_offered(cx: &mut TestAppContext) {
        // Given: a prompt that does not offer the keychain.
        let handle = open_window(cx);
        cx.update(install);
        let mut receiver = cx
            .update_window(handle, |_, window, cx| {
                prompt_password(window, cx, request(false))
            })
            .expect("window");
        dialog_open(cx, handle).await;
        // When: typing and pressing Enter.
        cx.update_window(handle, |_, window, cx| {
            assert!(
                window
                    .within("dialog")
                    .try_find("password-save-keychain")
                    .is_none()
            );
            window.within("dialog").click("password-input", cx);
            window.within("dialog").input("pw", cx);
            window.press("enter", cx);
        })
        .expect("window");
        // Then: the password is submitted without keychain saving.
        assert_eq!(
            receiver.try_recv().expect("open"),
            Some(Some(PasswordAnswer {
                password: Secret::new("pw".into()),
                save_to_keychain: false,
            }))
        );
    }

    #[gpui_kit::test]
    async fn cancel_resolves_none(cx: &mut TestAppContext) {
        // Given: an open prompt.
        let handle = open_window(cx);
        cx.update(install);
        let mut receiver = cx
            .update_window(handle, |_, window, cx| {
                prompt_password(window, cx, request(true))
            })
            .expect("window");
        dialog_open(cx, handle).await;
        // When: cancelling.
        cx.update_window(handle, |_, window, cx| {
            window.within("dialog").click("dialog-cancel", cx);
        })
        .expect("window");
        // Then: the answer is None.
        assert_eq!(receiver.try_recv().expect("open"), Some(None));
    }
}
