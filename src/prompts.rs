//! Prompt hooks for passwords, confirmations and notices.
//!
//! Views never open dialogs directly: they call [`prompt_password`], [`confirm`]
//! and [`notify`], which dispatch to the [`PromptHooks`] global. The defaults
//! ([`install_default`]) only support notices; the real dialogs replace the
//! hooks later by setting a new [`PromptHooks`] global.

use std::rc::Rc;
#[cfg(test)]
use std::{cell::RefCell, collections::VecDeque};

use futures::channel::oneshot;
use gpui_kit::component::{WindowExt as _, notification::Notification};
use gpui_kit::{App, Global, SharedString, Window};

use crate::catalog::model::ConnectionId;
use crate::db::model::Secret;

/// Asks for the password of a connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PasswordRequest {
    /// The connection being opened.
    pub connection: ConnectionId,
    /// Its display name.
    pub connection_name: SharedString,
    /// Whether the dialog offers to save the password to the keychain.
    pub offer_keychain: bool,
}

/// The user's answer to a [`PasswordRequest`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PasswordAnswer {
    /// The typed password.
    pub password: Secret,
    /// Whether to save it to the keychain.
    pub save_to_keychain: bool,
}

/// Asks the user to confirm an action, optionally offering alternatives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfirmRequest {
    /// Dialog title.
    pub title: SharedString,
    /// Dialog body.
    pub message: SharedString,
    /// Label of the confirm button.
    pub confirm_label: SharedString,
    /// Whether the action is destructive (styles the confirm button).
    pub destructive: bool,
    /// Labels of alternative buttons, answered as [`ConfirmChoice::Alternative`].
    pub alternatives: Vec<SharedString>,
}

/// The user's answer to a [`ConfirmRequest`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfirmChoice {
    /// The confirm button.
    Confirm,
    /// The alternative at this index.
    Alternative(usize),
    /// Dismissed.
    Cancel,
}

/// Severity of a [`Notice`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoticeLevel {
    /// Neutral information.
    Info,
    /// An operation succeeded.
    Success,
    /// Something needs attention.
    Warning,
    /// An operation failed.
    Error,
}

/// A transient message for the user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    /// Severity.
    pub level: NoticeLevel,
    /// Text shown.
    pub message: SharedString,
}

/// Hook answering a [`PasswordRequest`]; `None` means cancelled.
pub type PasswordHook =
    Rc<dyn Fn(&mut Window, &mut App, PasswordRequest) -> oneshot::Receiver<Option<PasswordAnswer>>>;
/// Hook answering a [`ConfirmRequest`].
pub type ConfirmHook =
    Rc<dyn Fn(&mut Window, &mut App, ConfirmRequest) -> oneshot::Receiver<ConfirmChoice>>;
/// Hook displaying a [`Notice`].
pub type NotifyHook = Rc<dyn Fn(&mut Window, &mut App, Notice)>;

/// The application's prompt implementation, as a GPUI global.
///
/// Replace it with `cx.set_global(PromptHooks { .. })` to change behavior.
#[derive(Clone)]
pub struct PromptHooks {
    /// Password prompt.
    pub password: PasswordHook,
    /// Confirmation prompt.
    pub confirm: ConfirmHook,
    /// Notification display.
    pub notify: NotifyHook,
}

impl Global for PromptHooks {}

/// Asks for a connection password through the installed hooks.
pub fn prompt_password(
    window: &mut Window,
    cx: &mut App,
    req: PasswordRequest,
) -> oneshot::Receiver<Option<PasswordAnswer>> {
    let hook = cx.global::<PromptHooks>().password.clone();
    hook(window, cx, req)
}

/// Asks for a confirmation through the installed hooks.
pub fn confirm(
    window: &mut Window,
    cx: &mut App,
    req: ConfirmRequest,
) -> oneshot::Receiver<ConfirmChoice> {
    let hook = cx.global::<PromptHooks>().confirm.clone();
    hook(window, cx, req)
}

/// Shows a notice through the installed hooks.
pub fn notify(window: &mut Window, cx: &mut App, notice: Notice) {
    let hook = cx.global::<PromptHooks>().notify.clone();
    hook(window, cx, notice);
}

fn resolved<T>(value: T) -> oneshot::Receiver<T> {
    let (sender, receiver) = oneshot::channel();
    // The receiver is still alive here, so sending cannot fail.
    drop(sender.send(value));
    receiver
}

/// Installs the default hooks: notices become toasts, passwords resolve to
/// `None` and confirmations to [`ConfirmChoice::Cancel`].
pub fn install_default(cx: &mut App) {
    cx.set_global(PromptHooks {
        password: Rc::new(|_, _, _| resolved(None)),
        confirm: Rc::new(|_, _, _| resolved(ConfirmChoice::Cancel)),
        notify: Rc::new(|window, cx, notice| {
            let note = match notice.level {
                NoticeLevel::Info => Notification::info(notice.message),
                NoticeLevel::Success => Notification::success(notice.message),
                NoticeLevel::Warning => Notification::warning(notice.message),
                NoticeLevel::Error => Notification::error(notice.message),
            };
            window.push_notification(note, cx);
        }),
    });
}

/// Scripted prompts for tests: queued answers, logged requests and notices.
///
/// Unanswered prompts resolve to `None` / [`ConfirmChoice::Cancel`].
#[cfg(test)]
#[derive(Default)]
pub struct ScriptedPrompts {
    passwords: RefCell<VecDeque<Option<PasswordAnswer>>>,
    confirms: RefCell<VecDeque<ConfirmChoice>>,
    password_log: RefCell<Vec<PasswordRequest>>,
    confirm_log: RefCell<Vec<ConfirmRequest>>,
    notice_log: RefCell<Vec<Notice>>,
}

#[cfg(test)]
impl ScriptedPrompts {
    /// Queues the answer to the next password prompt.
    pub fn queue_password(&self, answer: Option<PasswordAnswer>) {
        self.passwords.borrow_mut().push_back(answer);
    }

    /// Queues the answer to the next confirmation.
    pub fn queue_confirm(&self, choice: ConfirmChoice) {
        self.confirms.borrow_mut().push_back(choice);
    }

    /// Password requests seen so far.
    pub fn password_requests(&self) -> Vec<PasswordRequest> {
        self.password_log.borrow().clone()
    }

    /// Confirm requests seen so far.
    pub fn confirm_requests(&self) -> Vec<ConfirmRequest> {
        self.confirm_log.borrow().clone()
    }

    /// Notices shown so far.
    pub fn notices(&self) -> Vec<Notice> {
        self.notice_log.borrow().clone()
    }
}

/// Installs [`ScriptedPrompts`] as the prompt hooks.
#[cfg(test)]
pub fn install_scripted(cx: &mut App) -> Rc<ScriptedPrompts> {
    let scripted = Rc::new(ScriptedPrompts::default());
    let (password, confirm, notice) = (scripted.clone(), scripted.clone(), scripted.clone());
    cx.set_global(PromptHooks {
        password: Rc::new(move |_, _, req| {
            password.password_log.borrow_mut().push(req);
            let answer = password.passwords.borrow_mut().pop_front().flatten();
            resolved(answer)
        }),
        confirm: Rc::new(move |_, _, req| {
            confirm.confirm_log.borrow_mut().push(req);
            let choice = confirm.confirms.borrow_mut().pop_front();
            resolved(choice.unwrap_or(ConfirmChoice::Cancel))
        }),
        notify: Rc::new(move |_, _, note| notice.notice_log.borrow_mut().push(note)),
    });
    scripted
}

#[cfg(test)]
mod tests {
    use gpui_kit::TestAppContext;

    use super::{
        ConfirmChoice, ConfirmRequest, Notice, NoticeLevel, PasswordAnswer, PasswordRequest,
        confirm, install_default, install_scripted, notify, prompt_password,
    };
    use crate::catalog::model::ConnectionId;
    use crate::db::model::Secret;

    fn confirm_request(title: &str) -> ConfirmRequest {
        ConfirmRequest {
            title: title.to_owned().into(),
            message: "sure?".into(),
            confirm_label: "Delete".into(),
            destructive: true,
            alternatives: vec!["Keep".into()],
        }
    }

    fn password_request() -> PasswordRequest {
        PasswordRequest {
            connection: ConnectionId::new(),
            connection_name: "prod".into(),
            offer_keychain: true,
        }
    }

    #[gpui_kit::test]
    fn scripted_prompts_return_queued_answers_in_order(cx: &mut TestAppContext) {
        // Given: scripted prompts with queued answers.
        let prompts = cx.update(install_scripted);
        prompts.queue_confirm(ConfirmChoice::Alternative(0));
        prompts.queue_confirm(ConfirmChoice::Confirm);
        let answer = PasswordAnswer {
            password: Secret::new("pw".into()),
            save_to_keychain: true,
        };
        prompts.queue_password(Some(answer.clone()));
        let window = cx.add_empty_window();
        let request = password_request();
        // When: prompting.
        let (mut first, mut second, mut password) = window.update(|window, cx| {
            (
                confirm(window, cx, confirm_request("a")),
                confirm(window, cx, confirm_request("b")),
                prompt_password(window, cx, request.clone()),
            )
        });
        // Then: answers come in order and the requests are logged.
        assert_eq!(
            first.try_recv().unwrap(),
            Some(ConfirmChoice::Alternative(0))
        );
        assert_eq!(second.try_recv().unwrap(), Some(ConfirmChoice::Confirm));
        assert_eq!(password.try_recv().unwrap(), Some(Some(answer)));
        let titles: Vec<_> = prompts
            .confirm_requests()
            .into_iter()
            .map(|r| r.title)
            .collect();
        assert_eq!(titles, ["a", "b"]);
        assert_eq!(prompts.password_requests(), [request]);
    }

    #[gpui_kit::test]
    fn notify_reaches_the_installed_hook(cx: &mut TestAppContext) {
        // Given: scripted prompts.
        let prompts = cx.update(install_scripted);
        let window = cx.add_empty_window();
        let notice = Notice {
            level: NoticeLevel::Warning,
            message: "careful".into(),
        };
        // When: notifying.
        window.update(|window, cx| notify(window, cx, notice.clone()));
        // Then: the hook logged the notice.
        assert_eq!(prompts.notices(), [notice]);
    }

    #[gpui_kit::test]
    fn default_confirm_resolves_cancel(cx: &mut TestAppContext) {
        // Given: the default hooks.
        cx.update(gpui_kit::init);
        cx.update(install_default);
        let window = cx.add_empty_window();
        // When: confirming and asking for a password.
        let (mut choice, mut password) = window.update(|window, cx| {
            (
                confirm(window, cx, confirm_request("x")),
                prompt_password(window, cx, password_request()),
            )
        });
        // Then: confirm cancels and the password is declined.
        assert_eq!(choice.try_recv().unwrap(), Some(ConfirmChoice::Cancel));
        assert_eq!(password.try_recv().unwrap(), Some(None));
    }
}
