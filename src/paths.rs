//! Config dir resolution (Zed-style, XDG-aware) and config file paths.
//!
//! Only config files live here. Future data, state or cache files must honor
//! `XDG_DATA_HOME`, `XDG_STATE_HOME` and `XDG_CACHE_HOME` the same way
//! (absolute, non-empty values only) instead of piggybacking on the config dir.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

/// Operating-system family whose config dir conventions apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    MacOs,
    Linux,
    Windows,
}

impl Platform {
    /// The platform this binary was built for; other Unixes follow Linux rules.
    #[must_use]
    pub fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::MacOs
        } else if cfg!(target_os = "windows") {
            Self::Windows
        } else {
            Self::Linux
        }
    }
}

/// Failure to determine the config dir.
#[derive(Debug, thiserror::Error)]
pub enum PathsError {
    #[error("HOME is not set to an absolute path")]
    NoHome,
    #[error("APPDATA is not set")]
    NoAppData,
    #[error("cannot determine the current directory: {0}")]
    CurrentDir(#[from] io::Error),
}

/// Locations of the config files under one root directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigPaths {
    root: PathBuf,
}

impl ConfigPaths {
    #[must_use]
    pub fn at(root: PathBuf) -> Self {
        Self { root }
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub fn settings_file(&self) -> PathBuf {
        self.root.join("settings.json")
    }

    #[must_use]
    pub fn connections_file(&self) -> PathBuf {
        self.root.join("connections.json")
    }

    #[must_use]
    pub fn themes_dir(&self) -> PathBuf {
        self.root.join("themes")
    }

    /// Creates the root and the themes directory.
    ///
    /// # Errors
    /// Returns the I/O error if a directory cannot be created.
    pub fn ensure_dirs(&self) -> io::Result<()> {
        std::fs::create_dir_all(self.themes_dir())
    }
}

/// Resolves the config dir from the real environment and working directory.
///
/// # Errors
/// See [`resolve_with`]; also fails if the current directory is unavailable.
pub fn resolve() -> Result<ConfigPaths, PathsError> {
    resolve_with(
        |key| std::env::var_os(key),
        Platform::current(),
        &std::env::current_dir()?,
    )
}

/// Resolves the config dir with an injected environment lookup.
///
/// Precedence: `COLONNADE_CONFIG_DIR`, then an absolute `XDG_CONFIG_HOME`
/// (`<it>/colonnade`), then `$HOME/.config/colonnade` (macOS, Linux) or
/// `%APPDATA%/Colonnade` (Windows).
///
/// # Errors
/// [`PathsError::NoHome`] or [`PathsError::NoAppData`] when the platform
/// fallback variable is unusable.
pub fn resolve_with(
    lookup: impl Fn(&str) -> Option<OsString>,
    platform: Platform,
    cwd: &Path,
) -> Result<ConfigPaths, PathsError> {
    let non_empty = |key: &str| lookup(key).filter(|v| !v.is_empty()).map(PathBuf::from);

    if let Some(dir) = non_empty("COLONNADE_CONFIG_DIR") {
        return Ok(ConfigPaths::at(cwd.join(dir)));
    }
    if let Some(xdg) = non_empty("XDG_CONFIG_HOME").filter(|p| p.is_absolute()) {
        return Ok(ConfigPaths::at(xdg.join("colonnade")));
    }
    match platform {
        Platform::MacOs | Platform::Linux => non_empty("HOME")
            .filter(|p| p.is_absolute())
            .map(|home| ConfigPaths::at(home.join(".config").join("colonnade")))
            .ok_or(PathsError::NoHome),
        Platform::Windows => non_empty("APPDATA")
            .map(|appdata| ConfigPaths::at(appdata.join("Colonnade")))
            .ok_or(PathsError::NoAppData),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::path::PathBuf;

    fn base() -> PathBuf {
        std::env::temp_dir().join("colonnade-paths-test")
    }

    fn env(pairs: &[(&str, PathBuf)]) -> impl Fn(&str) -> Option<OsString> + use<> {
        let owned: Vec<(String, OsString)> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone().into_os_string()))
            .collect();
        move |key| owned.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
    }

    #[test]
    fn colonnade_config_dir_beats_xdg() {
        // Given both COLONNADE_CONFIG_DIR and XDG_CONFIG_HOME are set
        let lookup = env(&[
            ("COLONNADE_CONFIG_DIR", base().join("explicit")),
            ("XDG_CONFIG_HOME", base().join("xdg")),
        ]);
        // When resolving
        let paths = resolve_with(lookup, Platform::Linux, &base()).unwrap();
        // Then the explicit dir wins
        assert_eq!(paths.root(), base().join("explicit"));
    }

    #[test]
    fn relative_colonnade_config_dir_joins_cwd() {
        // Given a relative COLONNADE_CONFIG_DIR
        let lookup = env(&[("COLONNADE_CONFIG_DIR", PathBuf::from("rel/cfg"))]);
        // When resolving
        let paths = resolve_with(lookup, Platform::Linux, &base()).unwrap();
        // Then it is made absolute against cwd
        assert_eq!(paths.root(), base().join("rel/cfg"));
    }

    #[test]
    fn empty_colonnade_config_dir_is_ignored() {
        // Given an empty COLONNADE_CONFIG_DIR and an XDG dir
        let lookup = env(&[
            ("COLONNADE_CONFIG_DIR", PathBuf::new()),
            ("XDG_CONFIG_HOME", base().join("xdg")),
        ]);
        // When resolving
        let paths = resolve_with(lookup, Platform::Linux, &base()).unwrap();
        // Then XDG applies
        assert_eq!(paths.root(), base().join("xdg").join("colonnade"));
    }

    #[test]
    fn absolute_xdg_used_on_every_platform() {
        for platform in [Platform::Linux, Platform::MacOs, Platform::Windows] {
            // Given an absolute XDG_CONFIG_HOME
            let lookup = env(&[("XDG_CONFIG_HOME", base().join("xdg"))]);
            // When resolving
            let paths = resolve_with(lookup, platform, &base()).unwrap();
            // Then it is used with a lowercase colonnade suffix
            assert_eq!(paths.root(), base().join("xdg").join("colonnade"));
        }
    }

    #[test]
    fn relative_xdg_is_ignored() {
        // Given a relative XDG_CONFIG_HOME and a HOME
        let lookup = env(&[
            ("XDG_CONFIG_HOME", PathBuf::from("relative")),
            ("HOME", base().join("home")),
        ]);
        // When resolving
        let paths = resolve_with(lookup, Platform::Linux, &base()).unwrap();
        // Then the HOME fallback applies
        assert_eq!(
            paths.root(),
            base().join("home").join(".config").join("colonnade")
        );
    }

    #[test]
    fn empty_xdg_is_ignored() {
        // Given an empty XDG_CONFIG_HOME and a HOME
        let lookup = env(&[
            ("XDG_CONFIG_HOME", PathBuf::new()),
            ("HOME", base().join("home")),
        ]);
        // When resolving
        let paths = resolve_with(lookup, Platform::MacOs, &base()).unwrap();
        // Then the HOME fallback applies
        assert_eq!(
            paths.root(),
            base().join("home").join(".config").join("colonnade")
        );
    }

    #[test]
    fn unix_fallback_uses_home_dot_config() {
        for platform in [Platform::Linux, Platform::MacOs] {
            // Given only HOME
            let lookup = env(&[("HOME", base().join("home"))]);
            // When resolving
            let paths = resolve_with(lookup, platform, &base()).unwrap();
            // Then ~/.config/colonnade is used
            assert_eq!(
                paths.root(),
                base().join("home").join(".config").join("colonnade")
            );
        }
    }

    #[test]
    fn missing_home_is_an_error() {
        // Given no environment at all
        // When resolving on Linux
        let result = resolve_with(env(&[]), Platform::Linux, &base());
        // Then NoHome is reported
        assert!(matches!(result, Err(PathsError::NoHome)));
    }

    #[test]
    fn relative_home_is_an_error() {
        // Given a relative HOME
        let lookup = env(&[("HOME", PathBuf::from("home"))]);
        // When resolving on macOS
        let result = resolve_with(lookup, Platform::MacOs, &base());
        // Then NoHome is reported
        assert!(matches!(result, Err(PathsError::NoHome)));
    }

    #[test]
    fn windows_fallback_uses_appdata() {
        // Given only APPDATA
        let lookup = env(&[("APPDATA", base().join("appdata"))]);
        // When resolving on Windows
        let paths = resolve_with(lookup, Platform::Windows, &base()).unwrap();
        // Then %APPDATA%/Colonnade is used
        assert_eq!(paths.root(), base().join("appdata").join("Colonnade"));
    }

    #[test]
    fn missing_appdata_is_an_error() {
        // Given a HOME but no APPDATA
        let lookup = env(&[("HOME", base().join("home"))]);
        // When resolving on Windows
        let result = resolve_with(lookup, Platform::Windows, &base());
        // Then NoAppData is reported
        assert!(matches!(result, Err(PathsError::NoAppData)));
    }

    #[test]
    fn file_names_are_stable() {
        // Given a root
        let paths = ConfigPaths::at(base());
        // Then the file layout is fixed
        assert_eq!(paths.settings_file(), base().join("settings.json"));
        assert_eq!(paths.connections_file(), base().join("connections.json"));
        assert_eq!(paths.themes_dir(), base().join("themes"));
    }

    #[test]
    fn ensure_dirs_creates_root_and_themes() {
        // Given a not-yet-existing root
        let tmp = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::at(tmp.path().join("a").join("b"));
        // When ensuring dirs
        paths.ensure_dirs().unwrap();
        // Then both exist
        assert!(paths.root().is_dir());
        assert!(paths.themes_dir().is_dir());
    }
}
