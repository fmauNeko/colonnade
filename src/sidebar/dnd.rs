//! Sidebar drag and drop.

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::{
    Bounds, Context, IntoElement, Pixels, Render, SharedString, Window, div, prelude::*,
};

use super::node::{RowKind, SidebarNodeKey};
use crate::catalog::model::CatalogItem;
use crate::catalog::ops::{self, DropPosition, NodeRef};

/// Drag payload: a folder or a connection of the catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraggedCatalogItem {
    pub node: NodeRef,
    pub label: SharedString,
}

impl DraggedCatalogItem {
    /// The payload of a row; `None` for rows that cannot be dragged.
    #[must_use]
    pub fn of(key: &SidebarNodeKey, label: &SharedString) -> Option<Self> {
        let node = match key {
            SidebarNodeKey::Folder(id) => NodeRef::Folder(*id),
            SidebarNodeKey::Connection(id) => NodeRef::Connection(*id),
            SidebarNodeKey::Object { .. } | SidebarNodeKey::Placeholder { .. } => return None,
        };
        Some(Self {
            node,
            label: label.clone(),
        })
    }
}

/// The floating label shown while dragging.
pub struct DragPreview(pub SharedString);

impl Render for DragPreview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        div()
            .px_2()
            .py_1()
            .rounded_md()
            .bg(theme.popover)
            .text_color(theme.popover_foreground)
            .border_1()
            .border_color(theme.border)
            .child(self.0.clone())
    }
}

/// Where on a row the cursor drops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropZone {
    Before,
    After,
    Into,
}

/// The zone under `cursor_y` in a row of `bounds`: folders split into top
/// quarter, middle and bottom quarter; connections into halves; other rows
/// refuse drops.
#[must_use]
pub fn zone_for(target: RowKind, cursor_y: Pixels, bounds: Bounds<Pixels>) -> Option<DropZone> {
    let height = bounds.size.height.max(Pixels::from(1.));
    let fraction = ((cursor_y - bounds.origin.y) / height).clamp(0., 1.);
    match target {
        RowKind::Folder if fraction < 0.25 => Some(DropZone::Before),
        RowKind::Folder if fraction >= 0.75 => Some(DropZone::After),
        RowKind::Folder => Some(DropZone::Into),
        RowKind::Connection if fraction < 0.5 => Some(DropZone::Before),
        RowKind::Connection => Some(DropZone::After),
        RowKind::Database
        | RowKind::Schema
        | RowKind::Table
        | RowKind::View
        | RowKind::Placeholder(_) => None,
    }
}

/// The catalog position a drop on `target` in `zone` means.
#[must_use]
pub fn drop_position(target: &SidebarNodeKey, zone: DropZone) -> Option<DropPosition> {
    let node = match target {
        SidebarNodeKey::Folder(id) => NodeRef::Folder(*id),
        SidebarNodeKey::Connection(id) => NodeRef::Connection(*id),
        SidebarNodeKey::Object { .. } | SidebarNodeKey::Placeholder { .. } => return None,
    };
    match (zone, target) {
        (DropZone::Before, _) => Some(DropPosition::Before(node)),
        (DropZone::After, _) => Some(DropPosition::After(node)),
        (DropZone::Into, SidebarNodeKey::Folder(id)) => Some(DropPosition::Into(*id)),
        (DropZone::Into, _) => None,
    }
}

/// Whether moving `node` to `position` would succeed (it never does into its own subtree).
#[must_use]
pub fn is_valid_drop(items: &[CatalogItem], node: NodeRef, position: DropPosition) -> bool {
    if matches!(position, DropPosition::Before(target) | DropPosition::After(target) if target == node)
    {
        return false;
    }
    let mut scratch = items.to_vec();
    ops::move_node(&mut scratch, node, position).is_ok()
}

#[cfg(test)]
mod tests {
    use gpui_kit::{Bounds, Point, px, size};

    use super::{DropZone, drop_position, is_valid_drop, zone_for};
    use crate::catalog::model::{CatalogItem, ConnectionId, FolderId};
    use crate::catalog::ops::{DropPosition, NodeRef};
    use crate::sidebar::node::{PlaceholderKind, RowKind, SidebarNodeKey};

    fn row() -> Bounds<gpui_kit::Pixels> {
        Bounds::new(Point::new(px(0.), px(100.)), size(px(300.), px(28.)))
    }

    #[test]
    fn zone_table_for_folders_connections_and_other_rows() {
        let at = |y: f32| px(100. + y);
        // Folder rows: quarters Before / Into / After.
        assert_eq!(
            zone_for(RowKind::Folder, at(3.), row()),
            Some(DropZone::Before)
        );
        assert_eq!(
            zone_for(RowKind::Folder, at(14.), row()),
            Some(DropZone::Into)
        );
        assert_eq!(
            zone_for(RowKind::Folder, at(25.), row()),
            Some(DropZone::After)
        );
        // Connection rows: halves.
        assert_eq!(
            zone_for(RowKind::Connection, at(13.), row()),
            Some(DropZone::Before)
        );
        assert_eq!(
            zone_for(RowKind::Connection, at(15.), row()),
            Some(DropZone::After)
        );
        // Every other row refuses drops.
        for kind in [
            RowKind::Database,
            RowKind::Schema,
            RowKind::Table,
            RowKind::View,
            RowKind::Placeholder(PlaceholderKind::Empty),
        ] {
            assert_eq!(zone_for(kind, at(14.), row()), None, "{kind:?}");
        }
    }

    #[test]
    fn drop_positions_map_zones_to_catalog_positions() {
        let (folder, connection) = (FolderId::new(), ConnectionId::new());
        let f = SidebarNodeKey::Folder(folder);
        let c = SidebarNodeKey::Connection(connection);
        assert_eq!(
            drop_position(&f, DropZone::Before),
            Some(DropPosition::Before(NodeRef::Folder(folder)))
        );
        assert_eq!(
            drop_position(&f, DropZone::Into),
            Some(DropPosition::Into(folder))
        );
        assert_eq!(
            drop_position(&c, DropZone::After),
            Some(DropPosition::After(NodeRef::Connection(connection)))
        );
        assert_eq!(drop_position(&c, DropZone::Into), None);
    }

    #[test]
    fn folder_into_its_own_descendant_is_invalid() {
        // Given: folder F containing folder G.
        let (mut f, g) = (
            crate::catalog::model::Folder::new("F"),
            crate::catalog::model::Folder::new("G"),
        );
        let (f_id, g_id) = (f.id, g.id);
        f.items.push(CatalogItem::Folder(g));
        let items = vec![CatalogItem::Folder(f)];
        // Then: F cannot go into G; G can go to the root end.
        assert!(!is_valid_drop(
            &items,
            NodeRef::Folder(f_id),
            DropPosition::Into(g_id)
        ));
        assert!(is_valid_drop(
            &items,
            NodeRef::Folder(g_id),
            DropPosition::RootEnd
        ));
    }
}
