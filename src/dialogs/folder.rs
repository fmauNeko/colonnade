//! Folder dialog: create and rename catalog folders.

use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{WindowExt as _, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::{
    App, Context, Entity, IntoElement, Render, SharedString, Subscription, TestSupportExt as _,
    Window, div,
};

use crate::catalog::model::{CatalogItem, FolderId};
use crate::catalog::ops::{self, DropPosition, NodeRef};
use crate::catalog::store::{ConnectionStore, GlobalConnectionStore, StoreError};
use crate::prompts::{ConfirmChoice, ConfirmRequest, Notice, NoticeLevel, confirm, notify};

/// What saving the dialog does with the typed name.
type OnSave = Rc<
    dyn Fn(&mut ConnectionStore, &mut Context<ConnectionStore>, String) -> Result<(), StoreError>,
>;

fn store(cx: &App) -> Entity<ConnectionStore> {
    cx.global::<GlobalConnectionStore>().0.clone()
}

/// Body of the name dialog: one input and an error line.
struct FolderForm {
    input: Entity<InputState>,
    name_missing: bool,
    on_save: OnSave,
    _enter: Subscription,
}

impl FolderForm {
    /// Saves a non-empty name and closes; an empty name keeps the dialog open.
    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.input.read(cx).value().trim().to_owned();
        self.name_missing = name.is_empty();
        cx.notify();
        if self.name_missing {
            return;
        }
        let on_save = self.on_save.clone();
        let saved = store(cx).update(cx, |store, cx| on_save(store, cx, name));
        if let Err(error) = saved {
            notify(
                window,
                cx,
                Notice {
                    level: NoticeLevel::Error,
                    message: error.to_string().into(),
                },
            );
        }
        window.close_dialog(cx);
    }
}

impl Render for FolderForm {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .gap_2()
            .child(Input::new(&self.input).id("folder-name"))
            .when(self.name_missing, |this| {
                this.child(
                    div()
                        .id("folder-error")
                        .test_support()
                        .child("Enter a name"),
                )
            })
    }
}

fn open_name_dialog(
    title: &'static str,
    initial: &str,
    on_save: OnSave,
    window: &mut Window,
    cx: &mut App,
) {
    let initial = SharedString::from(initial.to_owned());
    let input = cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder("Folder name")
            .default_value(initial)
    });
    let form = cx.new(|cx| FolderForm {
        input: input.clone(),
        name_missing: false,
        on_save,
        _enter: cx.subscribe_in(
            &input,
            window,
            |this: &mut FolderForm, _, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.save(window, cx);
                }
            },
        ),
    });
    window.open_dialog(cx, move |dialog, _, _| {
        let save = {
            let form = form.clone();
            Button::new("dialog-save")
                .label("Save")
                .primary()
                .on_click(move |_, window, cx| form.update(cx, |form, cx| form.save(window, cx)))
        };
        let cancel = Button::new("dialog-cancel")
            .label("Cancel")
            .on_click(|_, window, cx| window.close_dialog(cx));
        dialog
            .title(title)
            .child(form.clone())
            .footer(DialogFooter::new().child(cancel).child(save))
    });
}

/// Opens the dialog creating a folder at `target`.
pub fn open_new(target: DropPosition, window: &mut Window, cx: &mut App) {
    let on_save: OnSave =
        Rc::new(move |store, cx, name| store.add_folder(cx, name, target).map(drop));
    open_name_dialog("New folder", "", on_save, window, cx);
}

/// Opens the dialog renaming `folder`.
pub fn open_rename(folder: FolderId, window: &mut Window, cx: &mut App) {
    let current = match ops::find(store(cx).read(cx).items(), NodeRef::Folder(folder)) {
        Some(CatalogItem::Folder(found)) => found.name.clone(),
        Some(CatalogItem::Connection(_)) | None => return,
    };
    let on_save: OnSave = Rc::new(move |store, cx, name| store.rename_folder(cx, folder, name));
    open_name_dialog("Rename folder", &current, on_save, window, cx);
}

/// Deletes `folder`: at once when empty, otherwise after a confirmation that
/// offers "Move contents up" as the alternative to deleting everything.
pub fn delete(folder: FolderId, window: &mut Window, cx: &mut App) {
    let (name, count, empty) = match ops::find(store(cx).read(cx).items(), NodeRef::Folder(folder))
    {
        Some(CatalogItem::Folder(found)) => (
            found.name.clone(),
            ops::connection_count(found),
            found.items.is_empty(),
        ),
        Some(CatalogItem::Connection(_)) | None => return,
    };
    if empty {
        apply_delete(folder, ConfirmChoice::Confirm, window, cx);
        return;
    }
    let receiver = confirm(
        window,
        cx,
        ConfirmRequest {
            title: "Delete folder".into(),
            message: format!("Folder \"{name}\" is not empty.").into(),
            confirm_label: format!("Delete folder and its {count} connections").into(),
            destructive: true,
            alternatives: vec!["Move contents up".into()],
        },
    );
    window
        .spawn(cx, async move |cx| {
            let choice = receiver.await.unwrap_or(ConfirmChoice::Cancel);
            // The window closing before the answer leaves nothing to update.
            drop(cx.update(|window, cx| apply_delete(folder, choice, window, cx)));
        })
        .detach();
}

fn apply_delete(folder: FolderId, choice: ConfirmChoice, window: &mut Window, cx: &mut App) {
    let result = match choice {
        ConfirmChoice::Confirm => store(cx)
            .update(cx, |store, cx| store.remove(cx, NodeRef::Folder(folder)))
            .map(drop),
        ConfirmChoice::Alternative(_) => store(cx).update(cx, |store, cx| {
            store.remove_folder_keep_contents(cx, folder)
        }),
        ConfirmChoice::Cancel => return,
    };
    if let Err(error) = result {
        notify(
            window,
            cx,
            Notice {
                level: NoticeLevel::Error,
                message: error.to_string().into(),
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{AnyWindowHandle, AppContext as _, TestAppContext};

    use super::super::confirm::{
        self,
        harness::{dialog_closed, dialog_open, open_window},
    };
    use super::{delete, open_new, open_rename};
    use crate::catalog::model::{
        CatalogItem, ConnectionProfile, Driver, Endpoint, FolderId, PasswordStorage,
    };
    use crate::catalog::ops::{DropPosition, NodeRef};
    use crate::catalog::store::{ConnectionStore, GlobalConnectionStore, init};
    use crate::db::model::TlsMode;
    use crate::paths::ConfigPaths;
    use crate::prompts::{ConfirmChoice, ConfirmRequest, install_scripted};

    fn setup(cx: &mut TestAppContext, dir: &tempfile::TempDir) -> AnyWindowHandle {
        let handle = open_window(cx);
        let paths = ConfigPaths::at(dir.path().to_path_buf());
        cx.update(|cx| init(cx, &paths));
        handle
    }

    fn connection(name: &str) -> ConnectionProfile {
        ConnectionProfile::new(
            name,
            Driver::Postgres,
            Endpoint::Server {
                host: "db.test".into(),
                port: 5432,
                user: "u".into(),
                database: None,
                tls: TlsMode::default(),
            },
            PasswordStorage::Ask,
        )
    }

    fn with_store<R>(
        cx: &mut TestAppContext,
        f: impl FnOnce(&mut ConnectionStore, &mut gpui_kit::Context<ConnectionStore>) -> R,
    ) -> R {
        cx.update(|cx| cx.global::<GlobalConnectionStore>().0.clone().update(cx, f))
    }

    fn folder_names(cx: &mut TestAppContext) -> Vec<String> {
        with_store(cx, |store, _| {
            store
                .items()
                .iter()
                .filter_map(|item| match item {
                    CatalogItem::Folder(folder) => Some(folder.name.clone()),
                    CatalogItem::Connection(_) => None,
                })
                .collect()
        })
    }

    fn first_folder(cx: &mut TestAppContext) -> FolderId {
        with_store(cx, |store, _| {
            store
                .items()
                .iter()
                .find_map(|item| match item {
                    CatalogItem::Folder(folder) => Some(folder.id),
                    CatalogItem::Connection(_) => None,
                })
                .expect("a folder")
        })
    }

    #[gpui_kit::test]
    async fn new_folder_is_added_and_empty_name_is_rejected(cx: &mut TestAppContext) {
        // Given: an empty catalog and the new-folder dialog.
        let dir = tempfile::tempdir().expect("tempdir");
        let handle = setup(cx, &dir);
        cx.update_window(handle, |_, window, cx| {
            open_new(DropPosition::RootEnd, window, cx);
        })
        .expect("window");
        dialog_open(cx, handle).await;
        // When: saving with an empty name, then with a typed one.
        cx.update_window(handle, |_, window, cx| {
            window.within("dialog").click("dialog-save", cx);
        })
        .expect("window");
        cx.run_until_parked();
        let rejected = cx
            .update_window(handle, |_, window, _| window.try_find("dialog").is_some())
            .expect("window");
        cx.update_window(handle, |_, window, cx| {
            window.within("dialog").click("folder-name", cx);
            window.within("dialog").input("Production", cx);
            window.within("dialog").click("dialog-save", cx);
        })
        .expect("window");
        // Then: the empty save kept the dialog open and the typed one added the folder.
        assert!(rejected);
        dialog_closed(cx, handle).await;
        assert_eq!(folder_names(cx), ["Production"]);
    }

    #[gpui_kit::test]
    async fn rename_replaces_the_name(cx: &mut TestAppContext) {
        // Given: a folder and the rename dialog.
        let dir = tempfile::tempdir().expect("tempdir");
        let handle = setup(cx, &dir);
        with_store(cx, |store, cx| {
            store.add_folder(cx, "Old", DropPosition::RootEnd)
        })
        .expect("add");
        let folder = first_folder(cx);
        cx.update_window(handle, |_, window, cx| open_rename(folder, window, cx))
            .expect("window");
        dialog_open(cx, handle).await;
        // When: replacing the prefilled name and saving.
        cx.update_window(handle, |_, window, cx| {
            window.within("dialog").click("folder-name", cx);
            for _ in 0.."Old".len() {
                window.press("backspace", cx);
            }
            window.within("dialog").input("New", cx);
            window.within("dialog").click("dialog-save", cx);
        })
        .expect("window");
        // Then: the folder is renamed.
        dialog_closed(cx, handle).await;
        assert_eq!(folder_names(cx), ["New"]);
    }

    #[gpui_kit::test]
    fn empty_folder_is_deleted_at_once(cx: &mut TestAppContext) {
        // Given: an empty folder and a scripted confirm that would cancel.
        let dir = tempfile::tempdir().expect("tempdir");
        let handle = setup(cx, &dir);
        let prompts = cx.update(install_scripted);
        with_store(cx, |store, cx| {
            store.add_folder(cx, "Empty", DropPosition::RootEnd)
        })
        .expect("add");
        let folder = first_folder(cx);
        // When: deleting it.
        cx.update_window(handle, |_, window, cx| delete(folder, window, cx))
            .expect("window");
        cx.run_until_parked();
        // Then: it is gone without any confirmation.
        assert_eq!(folder_names(cx).len(), 0);
        assert_eq!(prompts.confirm_requests().len(), 0);
    }

    fn folder_with_connection(cx: &mut TestAppContext) -> FolderId {
        with_store(cx, |store, cx| {
            store.add_folder(cx, "Work", DropPosition::RootEnd)
        })
        .expect("add");
        let folder = first_folder(cx);
        with_store(cx, |store, cx| {
            store.add_connection(cx, connection("a"), DropPosition::Into(folder))
        })
        .expect("add");
        folder
    }

    #[gpui_kit::test]
    fn delete_confirm_removes_folder_and_connections(cx: &mut TestAppContext) {
        // Given: a folder with a connection and a scripted Confirm.
        let dir = tempfile::tempdir().expect("tempdir");
        let handle = setup(cx, &dir);
        let prompts = cx.update(install_scripted);
        prompts.queue_confirm(ConfirmChoice::Confirm);
        let folder = folder_with_connection(cx);
        // When: deleting the folder.
        cx.update_window(handle, |_, window, cx| delete(folder, window, cx))
            .expect("window");
        cx.run_until_parked();
        // Then: the request offered "Move contents up" and everything is removed.
        let requests: Vec<ConfirmRequest> = prompts.confirm_requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].alternatives.len(), 1);
        assert!(requests[0].destructive);
        assert_eq!(with_store(cx, |store, _| store.items().len()), 0);
    }

    #[gpui_kit::test]
    fn move_contents_up_keeps_the_connections(cx: &mut TestAppContext) {
        // Given: a folder with a connection and a scripted Alternative(0).
        let dir = tempfile::tempdir().expect("tempdir");
        let handle = setup(cx, &dir);
        let prompts = cx.update(install_scripted);
        prompts.queue_confirm(ConfirmChoice::Alternative(0));
        let folder = folder_with_connection(cx);
        // When: deleting the folder.
        cx.update_window(handle, |_, window, cx| delete(folder, window, cx))
            .expect("window");
        cx.run_until_parked();
        // Then: only the folder is gone.
        let kept = with_store(cx, |store, _| {
            store
                .items()
                .iter()
                .map(CatalogItem::node_ref)
                .collect::<Vec<NodeRef>>()
        });
        assert_eq!(kept.len(), 1);
        assert!(matches!(kept[0], NodeRef::Connection(_)));
    }

    #[gpui_kit::test]
    async fn cancelled_delete_changes_nothing(cx: &mut TestAppContext) {
        // Given: the real confirm dialog over a folder with a connection.
        let dir = tempfile::tempdir().expect("tempdir");
        let handle = setup(cx, &dir);
        cx.update(confirm::install);
        let folder = folder_with_connection(cx);
        cx.update_window(handle, |_, window, cx| delete(folder, window, cx))
            .expect("window");
        dialog_open(cx, handle).await;
        // When: cancelling.
        cx.update_window(handle, |_, window, cx| {
            window.within("dialog").click("confirm-cancel", cx);
        })
        .expect("window");
        cx.run_until_parked();
        // Then: the folder is still there.
        assert_eq!(folder_names(cx), ["Work"]);
    }
}
