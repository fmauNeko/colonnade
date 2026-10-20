//! Application asset sources: the Colonnade icon set layered over the GPUI Kit defaults.

use std::borrow::Cow;

use gpui_kit::assets::Assets;
use gpui_kit::{AssetSource, Result, SharedString};

gpui_kit::assets::icon_assets!(
    pub ColonnadeIcons,
    [CircleStop, Database, Plug, Server, Table, Unplug]
);

/// Application asset source: the extra Colonnade icons first, then the GPUI Kit defaults.
pub struct AppAssets;

impl AssetSource for AppAssets {
    /// Loads `path` from the extra Colonnade icons, then from the default bundle.
    ///
    /// # Errors
    ///
    /// Returns an error only when an underlying source fails for a reason other
    /// than a missing asset; an unknown path resolves to `Ok(None)`.
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some(bytes) = ColonnadeIcons.load(path)? {
            return Ok(Some(bytes));
        }
        // The default bundle reports a missing asset as an error; this merged
        // source reports it as `Ok(None)`, like `ColonnadeIcons` does.
        Ok(Assets.load(path).ok().flatten())
    }

    /// Lists the assets under `path` from both sources, sorted and deduplicated.
    ///
    /// # Errors
    ///
    /// Returns an error when one of the underlying sources fails to list.
    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut paths = Assets.list(path)?;
        paths.extend(ColonnadeIcons.list(path)?);
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::AssetSource as _;
    use gpui_kit::assets::IconName;

    use super::AppAssets;

    #[test]
    fn extra_and_default_icons_resolve() {
        // Given: the application asset source.
        let source = AppAssets;

        // When: an extra icon and a default icon are loaded.
        let extra = source.load(&IconName::Database.path());
        let default = source.load(&IconName::Folder.path());

        // Then: both resolve to bytes.
        assert!(matches!(extra, Ok(Some(_))));
        assert!(matches!(default, Ok(Some(_))));
    }

    #[test]
    fn unknown_path_is_none() {
        // Given: the application asset source.
        let source = AppAssets;

        // When: an unknown icon path is loaded.
        let unknown = source.load("icons/does-not-exist.svg");

        // Then: nothing is found.
        assert!(matches!(unknown, Ok(None)));
    }

    #[test]
    fn list_contains_both_sources_once() {
        // Given: the application asset source.
        let source = AppAssets;

        // When: the icons directory is listed.
        let paths = source.list("icons").expect("the asset list is readable");

        // Then: an extra icon and a default icon each appear exactly once.
        let database = paths
            .iter()
            .filter(|path| path.as_str() == "icons/database.svg")
            .count();
        let folder = paths
            .iter()
            .filter(|path| path.as_str() == "icons/folder.svg")
            .count();
        assert_eq!(database, 1);
        assert_eq!(folder, 1);
    }
}
