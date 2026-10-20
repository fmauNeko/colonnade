//! Catalog tree operations.

use crate::catalog::model::{CatalogItem, ConnectionId, ConnectionProfile, Folder, FolderId};

/// A reference to a node of the tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NodeRef {
    Folder(FolderId),
    Connection(ConnectionId),
}

/// Where a node is dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropPosition {
    Before(NodeRef),
    After(NodeRef),
    Into(FolderId),
    RootEnd,
}

/// Why a tree operation failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum OpsError {
    #[error("node not found")]
    NotFound,
    #[error("a folder cannot be moved into itself or one of its descendants")]
    IntoDescendant,
}

/// Index path from the root list to a node.
type Path = Vec<usize>;

fn locate(items: &[CatalogItem], node: NodeRef) -> Option<Path> {
    for (index, item) in items.iter().enumerate() {
        if item.node_ref() == node {
            return Some(vec![index]);
        }
        if let CatalogItem::Folder(folder) = item
            && let Some(mut rest) = locate(&folder.items, node)
        {
            rest.insert(0, index);
            return Some(rest);
        }
    }
    None
}

fn list_at<'a>(items: &'a [CatalogItem], path: &[usize]) -> Option<&'a [CatalogItem]> {
    let mut list = items;
    for &index in path {
        match list.get(index)? {
            CatalogItem::Folder(folder) => list = &folder.items,
            CatalogItem::Connection(_) => return None,
        }
    }
    Some(list)
}

fn list_at_mut<'a>(
    items: &'a mut Vec<CatalogItem>,
    path: &[usize],
) -> Option<&'a mut Vec<CatalogItem>> {
    let mut list = items;
    for &index in path {
        match list.get_mut(index)? {
            CatalogItem::Folder(folder) => list = &mut folder.items,
            CatalogItem::Connection(_) => return None,
        }
    }
    Some(list)
}

/// Parent list path and index of a located node.
fn split(path: &[usize]) -> Result<(&[usize], usize), OpsError> {
    path.split_last()
        .map(|(index, parent)| (parent, *index))
        .ok_or(OpsError::NotFound)
}

/// Resolves a drop position to `(list path, index)` without modifying the tree.
fn resolve(items: &[CatalogItem], position: DropPosition) -> Result<(Path, usize), OpsError> {
    match position {
        DropPosition::Before(node) | DropPosition::After(node) => {
            let path = locate(items, node).ok_or(OpsError::NotFound)?;
            let (parent, index) = split(&path)?;
            let offset = usize::from(matches!(position, DropPosition::After(_)));
            Ok((parent.to_vec(), index + offset))
        }
        DropPosition::Into(folder) => {
            let path = locate(items, NodeRef::Folder(folder)).ok_or(OpsError::NotFound)?;
            let len = list_at(items, &path).ok_or(OpsError::NotFound)?.len();
            Ok((path, len))
        }
        DropPosition::RootEnd => Ok((Vec::new(), items.len())),
    }
}

/// Finds a node anywhere in the tree.
#[must_use]
pub fn find(items: &[CatalogItem], node: NodeRef) -> Option<&CatalogItem> {
    let path = locate(items, node)?;
    let (parent, index) = path.split_last().map(|(i, p)| (p, *i))?;
    list_at(items, parent)?.get(index)
}

/// Mutable variant of [`find`].
#[must_use]
pub fn find_mut(items: &mut Vec<CatalogItem>, node: NodeRef) -> Option<&mut CatalogItem> {
    let path = locate(items, node)?;
    let (index, parent) = path.split_last()?;
    list_at_mut(items, parent)?.get_mut(*index)
}

/// The connection with this id, if any.
#[must_use]
pub fn get_profile(items: &[CatalogItem], id: ConnectionId) -> Option<&ConnectionProfile> {
    match find(items, NodeRef::Connection(id))? {
        CatalogItem::Connection(profile) => Some(profile),
        CatalogItem::Folder(_) => None,
    }
}

/// Mutable variant of [`get_profile`].
#[must_use]
pub fn get_profile_mut(
    items: &mut Vec<CatalogItem>,
    id: ConnectionId,
) -> Option<&mut ConnectionProfile> {
    match find_mut(items, NodeRef::Connection(id))? {
        CatalogItem::Connection(profile) => Some(profile),
        CatalogItem::Folder(_) => None,
    }
}

/// All connections, depth-first in display order.
#[must_use]
pub fn connections(items: &[CatalogItem]) -> Vec<&ConnectionProfile> {
    let mut out = Vec::new();
    for item in items {
        match item {
            CatalogItem::Connection(profile) => out.push(profile),
            CatalogItem::Folder(folder) => out.extend(connections(&folder.items)),
        }
    }
    out
}

/// Inserts a new node at a position.
///
/// # Errors
/// [`OpsError::NotFound`] if the position refers to an unknown node.
pub fn insert(
    items: &mut Vec<CatalogItem>,
    item: CatalogItem,
    position: DropPosition,
) -> Result<(), OpsError> {
    let (path, index) = resolve(items, position)?;
    list_at_mut(items, &path)
        .ok_or(OpsError::NotFound)?
        .insert(index, item);
    Ok(())
}

/// Moves a node: removes it first, then re-resolves the target. Moving a node
/// before or after itself does nothing.
///
/// # Errors
/// [`OpsError::NotFound`] for an unknown node or target;
/// [`OpsError::IntoDescendant`] when a folder is dropped into itself or one of
/// its descendants. The tree is unchanged on error.
pub fn move_node(
    items: &mut Vec<CatalogItem>,
    node: NodeRef,
    position: DropPosition,
) -> Result<(), OpsError> {
    let node_path = locate(items, node).ok_or(OpsError::NotFound)?;
    let target = match position {
        DropPosition::Before(target) | DropPosition::After(target) => {
            let path = locate(items, target).ok_or(OpsError::NotFound)?;
            if path == node_path {
                return Ok(());
            }
            Some(path)
        }
        DropPosition::Into(folder) => {
            Some(locate(items, NodeRef::Folder(folder)).ok_or(OpsError::NotFound)?)
        }
        DropPosition::RootEnd => None,
    };
    if target.is_some_and(|path| path.starts_with(&node_path)) {
        return Err(OpsError::IntoDescendant);
    }
    let moved = remove(items, node)?;
    insert(items, moved, position)
}

/// Removes a node and returns it.
///
/// # Errors
/// [`OpsError::NotFound`] for an unknown node.
pub fn remove(items: &mut Vec<CatalogItem>, node: NodeRef) -> Result<CatalogItem, OpsError> {
    let path = locate(items, node).ok_or(OpsError::NotFound)?;
    let (parent, index) = split(&path)?;
    Ok(list_at_mut(items, parent)
        .ok_or(OpsError::NotFound)?
        .remove(index))
}

/// Removes a folder, splicing its children where it was.
///
/// # Errors
/// [`OpsError::NotFound`] for an unknown folder.
pub fn remove_folder_keep_contents(
    items: &mut Vec<CatalogItem>,
    folder: FolderId,
) -> Result<(), OpsError> {
    let path = locate(items, NodeRef::Folder(folder)).ok_or(OpsError::NotFound)?;
    let (parent, index) = split(&path)?;
    let list = list_at_mut(items, parent).ok_or(OpsError::NotFound)?;
    let Some(CatalogItem::Folder(found)) = list.get_mut(index) else {
        return Err(OpsError::NotFound);
    };
    let children = std::mem::take(&mut found.items);
    list.splice(index..=index, children);
    Ok(())
}

fn shift(items: &mut Vec<CatalogItem>, node: NodeRef, up: bool) -> Result<bool, OpsError> {
    let path = locate(items, node).ok_or(OpsError::NotFound)?;
    let (parent, index) = split(&path)?;
    let list = list_at_mut(items, parent).ok_or(OpsError::NotFound)?;
    let other = if up {
        match index.checked_sub(1) {
            Some(other) => other,
            None => return Ok(false),
        }
    } else {
        index + 1
    };
    if other >= list.len() {
        return Ok(false);
    }
    list.swap(index, other);
    Ok(true)
}

/// Swaps a node with its previous sibling; `Ok(false)` at the top edge.
///
/// # Errors
/// [`OpsError::NotFound`] for an unknown node.
pub fn move_up(items: &mut Vec<CatalogItem>, node: NodeRef) -> Result<bool, OpsError> {
    shift(items, node, true)
}

/// Swaps a node with its next sibling; `Ok(false)` at the bottom edge.
///
/// # Errors
/// [`OpsError::NotFound`] for an unknown node.
pub fn move_down(items: &mut Vec<CatalogItem>, node: NodeRef) -> Result<bool, OpsError> {
    shift(items, node, false)
}

/// Duplicates a connection right after the original, named `"<name> (copy)"`,
/// keeping its generated and custom colors.
///
/// # Errors
/// [`OpsError::NotFound`] for an unknown connection.
pub fn duplicate_connection(
    items: &mut Vec<CatalogItem>,
    id: ConnectionId,
) -> Result<ConnectionId, OpsError> {
    let mut copy = get_profile(items, id).ok_or(OpsError::NotFound)?.clone();
    copy.id = ConnectionId::new();
    copy.name = format!("{} (copy)", copy.name);
    let new_id = copy.id;
    insert(
        items,
        CatalogItem::Connection(copy),
        DropPosition::After(NodeRef::Connection(id)),
    )?;
    Ok(new_id)
}

fn folder_mut(items: &mut Vec<CatalogItem>, id: FolderId) -> Result<&mut Folder, OpsError> {
    match find_mut(items, NodeRef::Folder(id)) {
        Some(CatalogItem::Folder(folder)) => Ok(folder),
        Some(CatalogItem::Connection(_)) | None => Err(OpsError::NotFound),
    }
}

/// Renames a folder.
///
/// # Errors
/// [`OpsError::NotFound`] for an unknown folder.
pub fn rename_folder(
    items: &mut Vec<CatalogItem>,
    id: FolderId,
    name: impl Into<String>,
) -> Result<(), OpsError> {
    folder_mut(items, id)?.name = name.into();
    Ok(())
}

/// Expands or collapses a folder.
///
/// # Errors
/// [`OpsError::NotFound`] for an unknown folder.
pub fn set_folder_expanded(
    items: &mut Vec<CatalogItem>,
    id: FolderId,
    expanded: bool,
) -> Result<(), OpsError> {
    folder_mut(items, id)?.expanded = expanded;
    Ok(())
}

/// Number of connections in a folder, nested folders included.
#[must_use]
pub fn connection_count(folder: &Folder) -> usize {
    folder
        .items
        .iter()
        .map(|item| match item {
            CatalogItem::Connection(_) => 1,
            CatalogItem::Folder(inner) => connection_count(inner),
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::model::{Driver, Endpoint, HexColor, PasswordStorage};
    use crate::db::model::TlsMode;

    fn conn(name: &str) -> ConnectionProfile {
        ConnectionProfile::new(
            name,
            Driver::Postgres,
            Endpoint::Server {
                host: name.to_owned(),
                port: 5432,
                user: String::new(),
                database: None,
                tls: TlsMode::Prefer,
            },
            PasswordStorage::None,
        )
    }

    fn folder(name: &str, items: Vec<CatalogItem>) -> Folder {
        Folder {
            items,
            ..Folder::new(name)
        }
    }

    fn c(profile: &ConnectionProfile) -> CatalogItem {
        CatalogItem::Connection(profile.clone())
    }

    fn name_of(item: &CatalogItem) -> &str {
        match item {
            CatalogItem::Folder(f) => &f.name,
            CatalogItem::Connection(p) => &p.name,
        }
    }

    fn names(items: &[CatalogItem]) -> Vec<&str> {
        items.iter().map(name_of).collect()
    }

    fn conn_ref(profile: &ConnectionProfile) -> NodeRef {
        NodeRef::Connection(profile.id)
    }

    /// root: [a, F[b, G[c]], d]
    struct Tree {
        items: Vec<CatalogItem>,
        a: ConnectionProfile,
        b: ConnectionProfile,
        c: ConnectionProfile,
        d: ConnectionProfile,
        f: FolderId,
        g: FolderId,
    }

    fn tree() -> Tree {
        let (ca, cb, cc, cd) = (conn("a"), conn("b"), conn("c"), conn("d"));
        let grp = folder("G", vec![c(&cc)]);
        let outer = folder("F", vec![c(&cb), CatalogItem::Folder(grp.clone())]);
        let (outer_id, grp_id) = (outer.id, grp.id);
        Tree {
            items: vec![c(&ca), CatalogItem::Folder(outer), c(&cd)],
            a: ca,
            b: cb,
            c: cc,
            d: cd,
            f: outer_id,
            g: grp_id,
        }
    }

    fn folder_names(items: &[CatalogItem], id: FolderId) -> Vec<String> {
        match find(items, NodeRef::Folder(id)) {
            Some(CatalogItem::Folder(f)) => {
                names(&f.items).into_iter().map(str::to_owned).collect()
            }
            Some(CatalogItem::Connection(_)) | None => Vec::new(),
        }
    }

    #[test]
    fn find_locates_nested_nodes() {
        // Given a nested tree
        let t = tree();
        // When finding a deep connection, a folder and a missing id
        // Then the first two are found and the last is not
        assert!(matches!(
            find(&t.items, conn_ref(&t.c)),
            Some(CatalogItem::Connection(_))
        ));
        assert!(matches!(
            find(&t.items, NodeRef::Folder(t.g)),
            Some(CatalogItem::Folder(_))
        ));
        assert!(find(&t.items, NodeRef::Connection(ConnectionId::new())).is_none());
    }

    #[test]
    fn get_profile_and_mut() {
        // Given a nested tree
        let mut t = tree();
        // When renaming through get_profile_mut
        get_profile_mut(&mut t.items, t.c.id).unwrap().name = "renamed".to_owned();
        // Then get_profile sees it
        assert_eq!(get_profile(&t.items, t.c.id).unwrap().name, "renamed");
        assert!(get_profile(&t.items, ConnectionId::new()).is_none());
    }

    #[test]
    fn connections_are_depth_first() {
        // Given a nested tree
        let t = tree();
        // When listing connections
        let listed: Vec<&str> = connections(&t.items)
            .iter()
            .map(|p| p.name.as_str())
            .collect();
        // Then folders are walked in place
        assert_eq!(listed, ["a", "b", "c", "d"]);
    }

    #[test]
    fn insert_at_every_position() {
        // Given a nested tree
        let mut t = tree();
        let new = |n: &str| c(&conn(n));
        // When inserting before, after, into and at the end
        insert(
            &mut t.items,
            new("x1"),
            DropPosition::Before(conn_ref(&t.b)),
        )
        .unwrap();
        insert(&mut t.items, new("x2"), DropPosition::After(conn_ref(&t.a))).unwrap();
        insert(&mut t.items, new("x3"), DropPosition::Into(t.g)).unwrap();
        insert(&mut t.items, new("x4"), DropPosition::RootEnd).unwrap();
        // Then each lands where asked
        assert_eq!(names(&t.items), ["a", "x2", "F", "d", "x4"]);
        assert_eq!(folder_names(&t.items, t.f), ["x1", "b", "G"]);
        assert_eq!(folder_names(&t.items, t.g), ["c", "x3"]);
    }

    #[test]
    fn insert_with_missing_target_fails() {
        // Given a tree
        let mut t = tree();
        // When inserting relative to an unknown node
        let result = insert(
            &mut t.items,
            c(&conn("x")),
            DropPosition::Into(FolderId::new()),
        );
        // Then NotFound and nothing changes
        assert_eq!(result, Err(OpsError::NotFound));
        assert_eq!(t.items.len(), 3);
    }

    #[test]
    fn move_node_before_re_resolves_after_removal() {
        // Given a at index 0
        let mut t = tree();
        // When moving it after d
        move_node(
            &mut t.items,
            conn_ref(&t.a),
            DropPosition::After(conn_ref(&t.d)),
        )
        .unwrap();
        // Then it is last
        assert_eq!(names(&t.items), ["F", "d", "a"]);
    }

    #[test]
    fn move_node_into_folder_and_out() {
        // Given a tree
        let mut t = tree();
        // When moving d into G then c to the root end
        move_node(&mut t.items, conn_ref(&t.d), DropPosition::Into(t.g)).unwrap();
        move_node(&mut t.items, conn_ref(&t.c), DropPosition::RootEnd).unwrap();
        // Then both moved
        assert_eq!(folder_names(&t.items, t.g), ["d"]);
        assert_eq!(names(&t.items), ["a", "F", "c"]);
    }

    #[test]
    fn folder_cannot_move_into_descendant() {
        // Given F containing G
        let mut t = tree();
        let before = t.items.clone();
        // When moving F into G, next to a node in G, and into itself
        let into_g = move_node(&mut t.items, NodeRef::Folder(t.f), DropPosition::Into(t.g));
        let after_c = move_node(
            &mut t.items,
            NodeRef::Folder(t.f),
            DropPosition::After(conn_ref(&t.c)),
        );
        let into_self = move_node(&mut t.items, NodeRef::Folder(t.f), DropPosition::Into(t.f));
        // Then all fail and the tree is untouched
        assert_eq!(into_g, Err(OpsError::IntoDescendant));
        assert_eq!(after_c, Err(OpsError::IntoDescendant));
        assert_eq!(into_self, Err(OpsError::IntoDescendant));
        assert_eq!(t.items, before);
    }

    #[test]
    fn moving_onto_itself_is_a_no_op() {
        // Given a tree
        let mut t = tree();
        let before = t.items.clone();
        // When moving a node before and after itself
        let r1 = move_node(
            &mut t.items,
            conn_ref(&t.b),
            DropPosition::Before(conn_ref(&t.b)),
        );
        let r2 = move_node(
            &mut t.items,
            NodeRef::Folder(t.f),
            DropPosition::After(NodeRef::Folder(t.f)),
        );
        // Then Ok and unchanged
        assert_eq!((r1, r2), (Ok(()), Ok(())));
        assert_eq!(t.items, before);
    }

    #[test]
    fn move_with_missing_node_or_target_fails_without_change() {
        // Given a tree
        let mut t = tree();
        let before = t.items.clone();
        // When the node or the target does not exist
        let missing_node = move_node(
            &mut t.items,
            NodeRef::Connection(ConnectionId::new()),
            DropPosition::RootEnd,
        );
        let missing_target = move_node(
            &mut t.items,
            conn_ref(&t.a),
            DropPosition::Into(FolderId::new()),
        );
        // Then NotFound and the tree is untouched
        assert_eq!(missing_node, Err(OpsError::NotFound));
        assert_eq!(missing_target, Err(OpsError::NotFound));
        assert_eq!(t.items, before);
    }

    #[test]
    fn remove_returns_the_node() {
        // Given a tree
        let mut t = tree();
        // When removing a nested folder
        let removed = remove(&mut t.items, NodeRef::Folder(t.g)).unwrap();
        // Then it is returned with its content and gone from the tree
        assert!(matches!(&removed, CatalogItem::Folder(f) if f.id == t.g && f.items.len() == 1));
        assert!(find(&t.items, NodeRef::Folder(t.g)).is_none());
        assert_eq!(
            remove(&mut t.items, NodeRef::Folder(t.g)),
            Err(OpsError::NotFound)
        );
    }

    #[test]
    fn remove_folder_keep_contents_splices_in_place() {
        // Given F = [b, G]
        let mut t = tree();
        // When dissolving F
        remove_folder_keep_contents(&mut t.items, t.f).unwrap();
        // Then its children take its place, in order
        assert_eq!(names(&t.items), ["a", "b", "G", "d"]);
        assert_eq!(
            remove_folder_keep_contents(&mut t.items, t.f),
            Err(OpsError::NotFound)
        );
    }

    #[test]
    fn move_up_and_down_swap_siblings() {
        // Given a, F, d at the root
        let mut t = tree();
        // When moving d up and a down
        assert_eq!(move_up(&mut t.items, conn_ref(&t.d)), Ok(true));
        assert_eq!(move_down(&mut t.items, conn_ref(&t.a)), Ok(true));
        // Then order is d... a swapped with d? a,F,d -> a,d,F -> d,a,F
        assert_eq!(names(&t.items), ["d", "a", "F"]);
    }

    #[test]
    fn move_up_and_down_stop_at_the_edges() {
        // Given a first and d last
        let mut t = tree();
        // When moving past the edges
        // Then Ok(false) and nothing changes
        assert_eq!(move_up(&mut t.items, conn_ref(&t.a)), Ok(false));
        assert_eq!(move_down(&mut t.items, conn_ref(&t.d)), Ok(false));
        assert_eq!(names(&t.items), ["a", "F", "d"]);
        assert_eq!(move_up(&mut t.items, conn_ref(&t.b)), Ok(false));
        assert_eq!(
            move_down(&mut t.items, NodeRef::Connection(ConnectionId::new())),
            Err(OpsError::NotFound)
        );
    }

    #[test]
    fn move_up_stays_within_the_parent() {
        // Given F = [b, G]
        let mut t = tree();
        // When moving G up
        assert_eq!(move_up(&mut t.items, NodeRef::Folder(t.g)), Ok(true));
        // Then it swaps inside F only
        assert_eq!(folder_names(&t.items, t.f), ["G", "b"]);
        assert_eq!(names(&t.items), ["a", "F", "d"]);
    }

    #[test]
    fn duplicate_is_placed_after_the_original_with_same_colors() {
        // Given b with a custom color inside F
        let mut t = tree();
        get_profile_mut(&mut t.items, t.b.id).unwrap().color.custom =
            Some(HexColor::new(crate::color::Rgb { r: 9, g: 8, b: 7 }));
        // When duplicating
        let copy_id = duplicate_connection(&mut t.items, t.b.id).unwrap();
        // Then the copy follows b with a fresh id and identical colors
        assert_eq!(folder_names(&t.items, t.f), ["b", "b (copy)", "G"]);
        let original = get_profile(&t.items, t.b.id).unwrap().clone();
        let copy = get_profile(&t.items, copy_id).unwrap();
        assert_ne!(copy.id, original.id);
        assert_eq!(copy.color, original.color);
        assert_eq!(
            duplicate_connection(&mut t.items, ConnectionId::new()),
            Err(OpsError::NotFound)
        );
    }

    #[test]
    fn rename_and_expand_folder() {
        // Given a tree
        let mut t = tree();
        // When renaming and expanding G
        rename_folder(&mut t.items, t.g, "Renamed").unwrap();
        set_folder_expanded(&mut t.items, t.g, true).unwrap();
        // Then both stick
        assert!(matches!(
            find(&t.items, NodeRef::Folder(t.g)),
            Some(CatalogItem::Folder(f)) if f.name == "Renamed" && f.expanded
        ));
        assert_eq!(
            rename_folder(&mut t.items, FolderId::new(), "x"),
            Err(OpsError::NotFound)
        );
        assert_eq!(
            set_folder_expanded(&mut t.items, FolderId::new(), true),
            Err(OpsError::NotFound)
        );
    }

    #[test]
    fn connection_count_is_recursive() {
        // Given F = [b, G[c]]
        let t = tree();
        // When counting F and G
        let count = |id| match find(&t.items, NodeRef::Folder(id)) {
            Some(CatalogItem::Folder(f)) => connection_count(f),
            Some(CatalogItem::Connection(_)) | None => 0,
        };
        // Then nested connections count
        assert_eq!(count(t.f), 2);
        assert_eq!(count(t.g), 1);
    }
}
