//! Theme catalog: available themes and selection.
//!
//! The catalog lists Kit's built-in themes plus every `*.json` file directly
//! in the themes directory (Kit `ThemeSet` or Zed theme family). Bad files
//! become [`ThemeLoadError`]s and never abort loading.

use std::path::{Path, PathBuf};
use std::rc::Rc;

use gpui_kit::component::highlighter::HighlightTheme;
use gpui_kit::component::{Theme, ThemeConfig, ThemeMode, ThemeRegistry, ThemeSet};
use gpui_kit::{App, Global, SharedString, WindowAppearance};

use crate::paths::ConfigPaths;
use crate::settings::{ThemeModeSetting, ThemeSettings};
use crate::themes::zed::{self, ThemeFileFormat};

/// Where a theme comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThemeSource {
    /// One of Kit's default themes.
    BuiltIn,
    /// A file in the themes directory.
    File(PathBuf),
}

/// One selectable theme.
#[derive(Debug, Clone)]
pub struct ThemeEntry {
    /// Theme name, unique in the catalog.
    pub name: SharedString,
    /// Light or dark.
    pub mode: ThemeMode,
    /// Origin of the theme.
    pub source: ThemeSource,
    /// Kit configuration, always carrying a highlight style.
    pub config: Rc<ThemeConfig>,
}

/// A theme file (or one theme in it) that could not be loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeLoadError {
    /// Offending file.
    pub file: PathBuf,
    /// Human-readable reason.
    pub message: String,
}

/// Outcome of a [`reload`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReloadReport {
    /// Whether the list of load errors differs from before the reload.
    pub errors_changed: bool,
}

/// Global list of available themes and load errors.
#[derive(Debug)]
pub struct ThemeCatalog {
    dir: PathBuf,
    entries: Vec<ThemeEntry>,
    errors: Vec<ThemeLoadError>,
}

impl Global for ThemeCatalog {}

impl ThemeCatalog {
    /// The catalog global.
    #[must_use]
    pub fn global(cx: &App) -> &Self {
        cx.global::<Self>()
    }

    /// Looks a theme up by exact name.
    #[must_use]
    pub fn find(&self, name: &str) -> Option<&ThemeEntry> {
        self.entries
            .iter()
            .find(|entry| entry.name.as_ref() == name)
    }

    /// Built-ins first, then file themes in file-name order.
    #[must_use]
    pub fn entries(&self) -> &[ThemeEntry] {
        &self.entries
    }

    /// Errors of the last scan, in file-name order.
    #[must_use]
    pub fn errors(&self) -> &[ThemeLoadError] {
        &self.errors
    }
}

/// Installs the catalog global and scans the themes directory once.
pub fn init(cx: &mut App, paths: &ConfigPaths) {
    let mut built_ins: Vec<ThemeEntry> = ThemeRegistry::global(cx)
        .default_themes()
        .values()
        .map(|config| entry(config, ThemeSource::BuiltIn))
        .collect();
    built_ins.sort_by_key(|built_in| built_in.mode.is_dark());
    let dir = paths.themes_dir();
    let (entries, errors) = scan(&dir, built_ins);
    cx.set_global(ThemeCatalog {
        dir,
        entries,
        errors,
    });
}

/// Rescans the themes directory, replacing every file theme.
pub fn reload(cx: &mut App) -> ReloadReport {
    let catalog = cx.global::<ThemeCatalog>();
    let built_ins: Vec<ThemeEntry> = catalog
        .entries
        .iter()
        .filter(|entry| entry.source == ThemeSource::BuiltIn)
        .cloned()
        .collect();
    let (entries, errors) = scan(&catalog.dir.clone(), built_ins);
    let catalog = cx.global_mut::<ThemeCatalog>();
    let errors_changed = catalog.errors != errors;
    catalog.entries = entries;
    catalog.errors = errors;
    ReloadReport { errors_changed }
}

/// Mode to display: the setting, or the window appearance for `System`.
#[must_use]
pub fn resolve_mode(setting: ThemeModeSetting, appearance: WindowAppearance) -> ThemeMode {
    match setting {
        ThemeModeSetting::System => ThemeMode::from(appearance),
        ThemeModeSetting::Light => ThemeMode::Light,
        ThemeModeSetting::Dark => ThemeMode::Dark,
    }
}

/// Applies the saved selection to Kit's global theme.
///
/// Returns the selected names that are missing (or of the wrong mode); those
/// fall back to `Default Light` / `Default Dark`.
pub fn apply_selection(
    cx: &mut App,
    settings: &ThemeSettings,
    appearance: WindowAppearance,
) -> Vec<SharedString> {
    let mut missing = Vec::new();
    let mut pick = |name: &str, mode: ThemeMode, fallback: &Rc<ThemeConfig>| {
        let found = ThemeCatalog::global(cx)
            .find(name)
            .filter(|entry| entry.mode == mode);
        if let Some(entry) = found {
            entry.config.clone()
        } else {
            missing.push(SharedString::from(name.to_owned()));
            fallback.clone()
        }
    };
    let registry = ThemeRegistry::global(cx);
    let (default_light, default_dark) = (
        registry.default_light_theme().clone(),
        registry.default_dark_theme().clone(),
    );
    let light = pick(&settings.light, ThemeMode::Light, &default_light);
    let dark = pick(&settings.dark, ThemeMode::Dark, &default_dark);
    let theme = Theme::global_mut(cx);
    theme.light_theme = light;
    theme.dark_theme = dark;
    Theme::change(resolve_mode(settings.mode, appearance), None, cx);
    missing
}

/// Applies a theme to the global theme without changing the selection.
pub fn preview(cx: &mut App, entry: &ThemeEntry) {
    Theme::update(cx, |theme| theme.apply_config(&entry.config));
}

fn entry(config: &Rc<ThemeConfig>, source: ThemeSource) -> ThemeEntry {
    let config = if config.highlight.is_some() {
        config.clone()
    } else {
        let defaults = if config.mode.is_dark() {
            HighlightTheme::default_dark()
        } else {
            HighlightTheme::default_light()
        };
        let mut patched = ThemeConfig::clone(config);
        patched.highlight = Some(defaults.style.clone());
        Rc::new(patched)
    };
    ThemeEntry {
        name: config.name.clone(),
        mode: config.mode,
        source,
        config,
    }
}

fn scan(dir: &Path, mut entries: Vec<ThemeEntry>) -> (Vec<ThemeEntry>, Vec<ThemeLoadError>) {
    let mut errors = Vec::new();
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|item| item.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json") && path.is_file())
        .collect();
    files.sort();
    for file in files {
        for loaded in load_file(&file) {
            let mut fail = |message: String| {
                errors.push(ThemeLoadError {
                    file: file.clone(),
                    message,
                });
            };
            match loaded {
                Err(message) => fail(message),
                Ok(config) if entries.iter().any(|known| known.name == config.name) => {
                    fail(format!("duplicate theme name {:?}", config.name));
                }
                Ok(config) => {
                    entries.push(entry(&Rc::new(config), ThemeSource::File(file.clone())));
                }
            }
        }
    }
    (entries, errors)
}

fn load_file(file: &Path) -> Vec<Result<ThemeConfig, String>> {
    let text = match std::fs::read_to_string(file) {
        Ok(text) => text,
        Err(error) => return vec![Err(format!("cannot read file: {error}"))],
    };
    let value: serde_json::Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(error) => return vec![Err(format!("invalid JSON: {error}"))],
    };
    match zed::detect_format(&value) {
        ThemeFileFormat::Zed => zed::convert_family(&value)
            .into_iter()
            .map(|result| result.map_err(|error| error.to_string()))
            .collect(),
        ThemeFileFormat::Kit => match serde_json::from_value::<ThemeSet>(value) {
            Ok(set) => set.themes.into_iter().map(Ok).collect(),
            Err(error) => vec![Err(format!("not a theme file: {error}"))],
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::ConfigPaths;
    use crate::settings::{ThemeModeSetting, ThemeSettings};
    use gpui_kit::TestAppContext;
    use gpui_kit::WindowAppearance;
    use gpui_kit::component::{ActiveTheme as _, ThemeMode};
    use std::sync::Arc;

    const KIT: &str = include_str!("fixtures/kit-sample.json");
    const ZED: &str = include_str!("fixtures/zed-sample.json");

    fn dir_with(files: &[(&str, &str)]) -> (tempfile::TempDir, ConfigPaths) {
        let root = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::at(root.path().to_path_buf());
        std::fs::create_dir_all(paths.themes_dir()).unwrap();
        for (name, text) in files {
            std::fs::write(paths.themes_dir().join(name), text).unwrap();
        }
        (root, paths)
    }

    fn start(cx: &mut TestAppContext, paths: &ConfigPaths) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            init(cx, paths);
        });
    }

    fn names(cx: &mut TestAppContext) -> Vec<String> {
        cx.update(|cx| {
            ThemeCatalog::global(cx)
                .entries()
                .iter()
                .map(|entry| entry.name.to_string())
                .collect()
        })
    }

    fn error_count(cx: &mut TestAppContext) -> usize {
        cx.update(|cx| ThemeCatalog::global(cx).errors().len())
    }

    fn theme(mode: ThemeModeSetting, light: &str, dark: &str) -> ThemeSettings {
        ThemeSettings {
            mode,
            light: light.to_owned(),
            dark: dark.to_owned(),
        }
    }

    #[gpui_kit::test]
    fn built_ins_are_listed(cx: &mut TestAppContext) {
        // Given: an empty themes directory.
        let (_root, paths) = dir_with(&[]);
        // When: the catalog is initialised.
        start(cx, &paths);
        // Then: both built-in themes are present and no error is reported.
        let listed = names(cx);
        assert!(listed.contains(&"Default Light".to_owned()));
        assert!(listed.contains(&"Default Dark".to_owned()));
        assert_eq!(error_count(cx), 0);
    }

    #[gpui_kit::test]
    fn kit_and_zed_files_load(cx: &mut TestAppContext) {
        // Given: one Kit file and one Zed family file.
        let (_root, paths) = dir_with(&[("a-kit.json", KIT), ("b-zed.json", ZED)]);
        // When: the catalog is initialised.
        start(cx, &paths);
        // Then: all three user themes are listed as file themes.
        let listed = names(cx);
        for expected in ["Kit Night", "Sample Day", "Sample Night"] {
            assert!(listed.contains(&expected.to_owned()), "{expected}");
        }
        cx.update(|cx| {
            let entry = ThemeCatalog::global(cx).find("Kit Night").unwrap();
            assert!(matches!(entry.source, ThemeSource::File(_)));
            assert_eq!(entry.mode, ThemeMode::Dark);
        });
        assert_eq!(error_count(cx), 0);
    }

    #[gpui_kit::test]
    fn invalid_files_are_reported_not_fatal(cx: &mut TestAppContext) {
        // Given: malformed JSON, a wrong-type file, a duplicate and a built-in clash.
        let clash = r#"{"themes":[{"name":"Default Light","mode":"light","colors":{}}]}"#;
        let (_root, paths) = dir_with(&[
            ("a-valid.json", KIT),
            ("b-malformed.json", "{ not json"),
            ("c-wrong-type.json", "[1, 2]"),
            ("d-duplicate.json", KIT),
            ("e-clash.json", clash),
            ("f-zed.json", ZED),
            ("ignored.txt", "x"),
        ]);
        // When: the catalog is initialised.
        start(cx, &paths);
        // Then: each bad file yields exactly one error and valid themes still load.
        assert_eq!(error_count(cx), 4);
        cx.update(|cx| {
            let files: Vec<String> = ThemeCatalog::global(cx)
                .errors()
                .iter()
                .map(|error| {
                    error
                        .file
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned()
                })
                .collect();
            assert_eq!(
                files,
                [
                    "b-malformed.json",
                    "c-wrong-type.json",
                    "d-duplicate.json",
                    "e-clash.json"
                ]
            );
        });
        let listed = names(cx);
        assert_eq!(listed.iter().filter(|name| *name == "Kit Night").count(), 1);
        assert_eq!(
            listed
                .iter()
                .filter(|name| *name == "Default Light")
                .count(),
            1
        );
        assert!(listed.contains(&"Sample Night".to_owned()));
    }

    #[gpui_kit::test]
    fn kit_theme_without_highlight_gets_default_highlight(cx: &mut TestAppContext) {
        // Given: the Zed dark theme applied (it carries its own highlight).
        let (_root, paths) = dir_with(&[("a-kit.json", KIT), ("b-zed.json", ZED)]);
        start(cx, &paths);
        let zed_dark = theme(ThemeModeSetting::Dark, "Default Light", "Sample Night");
        let before = cx.update(|cx| {
            apply_selection(cx, &zed_dark, WindowAppearance::Dark);
            cx.theme().highlight_theme.clone()
        });
        // When: switching to the Kit dark theme that has no highlight.
        let kit_dark = theme(ThemeModeSetting::Dark, "Default Light", "Kit Night");
        let after = cx.update(|cx| {
            apply_selection(cx, &kit_dark, WindowAppearance::Dark);
            cx.theme().highlight_theme.clone()
        });
        // Then: the syntax colors changed to Kit's default dark highlight.
        assert!(!Arc::ptr_eq(&before, &after));
        assert_eq!(
            after.style.syntax.clone(),
            HighlightTheme::default_dark().style.syntax.clone()
        );
    }

    #[gpui_kit::test]
    fn system_mode_follows_dark_appearance(cx: &mut TestAppContext) {
        // Given: a catalog with a custom dark theme and System mode.
        let (_root, paths) = dir_with(&[("a-kit.json", KIT)]);
        start(cx, &paths);
        let settings = theme(ThemeModeSetting::System, "Default Light", "Kit Night");
        // When: the system appearance is dark.
        let missing = cx.update(|cx| apply_selection(cx, &settings, WindowAppearance::Dark));
        // Then: the dark theme is active and nothing is missing.
        assert_eq!(missing, Vec::<SharedString>::new());
        cx.update(|cx| {
            assert!(cx.theme().is_dark());
            assert_eq!(cx.theme().theme_name().as_ref(), "Kit Night");
        });
    }

    #[gpui_kit::test]
    fn light_mode_ignores_appearance(cx: &mut TestAppContext) {
        // Given: Light mode with a custom light theme.
        let (_root, paths) = dir_with(&[("b-zed.json", ZED)]);
        start(cx, &paths);
        let settings = theme(ThemeModeSetting::Light, "Sample Day", "Default Dark");
        // When: the system appearance is dark.
        cx.update(|cx| apply_selection(cx, &settings, WindowAppearance::Dark));
        // Then: the light theme is active.
        cx.update(|cx| {
            assert!(!cx.theme().is_dark());
            assert_eq!(cx.theme().theme_name().as_ref(), "Sample Day");
        });
    }

    #[gpui_kit::test]
    fn missing_theme_falls_back_and_is_reported(cx: &mut TestAppContext) {
        // Given: a selection naming a theme that does not exist.
        let (_root, paths) = dir_with(&[]);
        start(cx, &paths);
        let settings = theme(ThemeModeSetting::Dark, "Default Light", "Gone Theme");
        // When: applying it.
        let missing = cx.update(|cx| apply_selection(cx, &settings, WindowAppearance::Light));
        // Then: the missing name is reported and Default Dark is active.
        assert_eq!(missing, vec![SharedString::from("Gone Theme")]);
        cx.update(|cx| {
            assert!(cx.theme().is_dark());
            assert_eq!(cx.theme().theme_name().as_ref(), "Default Dark");
        });
    }

    #[gpui_kit::test]
    fn deleted_file_is_removed_on_reload(cx: &mut TestAppContext) {
        // Given: a loaded Kit file.
        let (_root, paths) = dir_with(&[("a-kit.json", KIT)]);
        start(cx, &paths);
        assert!(names(cx).contains(&"Kit Night".to_owned()));
        // When: the file is deleted and the catalog reloaded.
        std::fs::remove_file(paths.themes_dir().join("a-kit.json")).unwrap();
        cx.update(reload);
        // Then: its theme is gone.
        assert!(!names(cx).contains(&"Kit Night".to_owned()));
    }

    #[gpui_kit::test]
    fn reload_reports_whether_errors_changed(cx: &mut TestAppContext) {
        // Given: a catalog without errors.
        let (_root, paths) = dir_with(&[]);
        start(cx, &paths);
        // When: a malformed file appears, then the same state is reloaded.
        std::fs::write(paths.themes_dir().join("bad.json"), "{").unwrap();
        let first = cx.update(reload);
        let second = cx.update(reload);
        // Then: only the first reload reports changed errors.
        assert!(first.errors_changed);
        assert!(!second.errors_changed);
    }

    #[test]
    fn resolve_mode_follows_setting_then_appearance() {
        // Given / When / Then: System follows the appearance, others ignore it.
        let cases = [
            (
                ThemeModeSetting::System,
                WindowAppearance::Dark,
                ThemeMode::Dark,
            ),
            (
                ThemeModeSetting::System,
                WindowAppearance::VibrantLight,
                ThemeMode::Light,
            ),
            (
                ThemeModeSetting::Light,
                WindowAppearance::Dark,
                ThemeMode::Light,
            ),
            (
                ThemeModeSetting::Dark,
                WindowAppearance::Light,
                ThemeMode::Dark,
            ),
        ];
        for (setting, appearance, expected) in cases {
            assert_eq!(resolve_mode(setting, appearance), expected);
        }
    }
}
