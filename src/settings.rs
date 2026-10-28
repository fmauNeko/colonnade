//! Settings model and store.

use std::io;
use std::path::PathBuf;

use gpui_kit::prelude::*;
use gpui_kit::{App, Context, Entity, EventEmitter, Global};
use serde::{Deserialize, Serialize};

use crate::paths::ConfigPaths;
use crate::persist::{self, LoadOutcome, Versioned};

/// Theme selection saved in settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ThemeModeSetting {
    #[default]
    System,
    Light,
    Dark,
}

/// Theme names and selection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ThemeSettings {
    pub mode: ThemeModeSetting,
    pub light: String,
    pub dark: String,
}

impl Default for ThemeSettings {
    fn default() -> Self {
        Self {
            mode: ThemeModeSetting::System,
            light: "Default Light".to_owned(),
            dark: "Default Dark".to_owned(),
        }
    }
}

/// Window arrangement behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum WindowMode {
    #[default]
    #[serde(rename = "single")]
    Single,
    #[serde(rename = "per_connection")]
    PerConnection,
}

/// User-configurable application settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub version: u32,
    pub theme: ThemeSettings,
    pub window_mode: WindowMode,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            version: Self::CURRENT_VERSION,
            theme: ThemeSettings::default(),
            window_mode: WindowMode::Single,
        }
    }
}

impl Versioned for Settings {
    const CURRENT_VERSION: u32 = 1;
}

/// Change notification emitted by [`SettingsStore`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsEvent {
    Changed { theme: bool, window_mode: bool },
}

impl EventEmitter<SettingsEvent> for SettingsStore {}

/// Deferred information about a settings file that could not be used normally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadNotice {
    Corrupt { backup: PathBuf },
    NewerVersion,
}

/// Failures returned when updating settings.
#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error("settings are read-only because the file uses a newer version")]
    ReadOnly,
    #[error("could not save settings: {0}")]
    Io(#[from] io::Error),
}

/// GPUI entity holding settings and persistence state.
pub struct SettingsStore {
    file: PathBuf,
    settings: Settings,
    read_only: bool,
    load_notice: Option<LoadNotice>,
}

/// GPUI global handle to the settings store.
pub struct GlobalSettings(pub Entity<SettingsStore>);

impl Global for GlobalSettings {}

impl SettingsStore {
    /// Returns the current settings.
    #[must_use]
    pub const fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Returns whether a newer file version prevents writes.
    #[must_use]
    pub const fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// Takes the pending load notice, if any.
    #[must_use]
    pub fn take_load_notice(&mut self) -> Option<LoadNotice> {
        self.load_notice.take()
    }

    /// Updates settings, saves changes and emits a change event.
    ///
    /// # Errors
    /// Returns [`SettingsError::ReadOnly`] for files from a newer version, or
    /// [`SettingsError::Io`] when persistence fails.
    pub fn update(
        &mut self,
        cx: &mut Context<Self>,
        update: impl FnOnce(&mut Settings),
    ) -> Result<(), SettingsError> {
        if self.read_only {
            return Err(SettingsError::ReadOnly);
        }
        let previous = self.settings.clone();
        update(&mut self.settings);
        let theme = previous.theme != self.settings.theme;
        let window_mode = previous.window_mode != self.settings.window_mode;
        if !theme && !window_mode {
            return Ok(());
        }
        if let Err(error) = persist::save_json(&self.file, &self.settings) {
            self.settings = previous;
            return Err(error.into());
        }
        cx.emit(SettingsEvent::Changed { theme, window_mode });
        Ok(())
    }
}

/// Loads settings and installs the store as a GPUI global.
pub fn init(cx: &mut App, paths: &ConfigPaths) {
    let file = paths.settings_file();
    let (settings, read_only, load_notice) = match persist::load_json::<Settings>(&file) {
        Ok(LoadOutcome::Missing) | Err(_) => (Settings::default(), false, None),
        Ok(LoadOutcome::Loaded(settings)) => (settings, false, None),
        Ok(LoadOutcome::Corrupt { backup, .. }) => (
            Settings::default(),
            false,
            Some(LoadNotice::Corrupt { backup }),
        ),
        Ok(LoadOutcome::NewerVersion { .. }) => {
            (Settings::default(), true, Some(LoadNotice::NewerVersion))
        }
    };
    let entity = cx.new(|_| SettingsStore {
        file,
        settings,
        read_only,
        load_notice,
    });
    cx.set_global(GlobalSettings(entity));
}

#[cfg(test)]
mod tests {
    use std::fs;

    use gpui_kit::TestAppContext;

    use crate::paths::ConfigPaths;

    use super::{GlobalSettings, Settings, ThemeModeSetting, WindowMode, init};

    fn setup(cx: &mut TestAppContext) -> tempfile::TempDir {
        let directory = tempfile::tempdir().expect("temporary directory is created");
        let paths = ConfigPaths::at(directory.path().to_path_buf());
        cx.update(|cx| init(cx, &paths));
        directory
    }

    #[gpui_kit::test]
    fn defaults_when_file_is_missing() {
        // Given a settings store with no existing file.
        let mut cx = TestAppContext::single();
        let directory = setup(&mut cx);
        // When reading its settings.
        let settings = cx.update(|cx| cx.global::<GlobalSettings>().0.read(cx).settings().clone());
        // Then defaults are available without creating the file.
        assert_eq!(settings, Settings::default());
        assert!(!directory.path().join("settings.json").exists());
    }

    #[gpui_kit::test]
    fn update_persists_and_emits_changed_parts() {
        // Given a missing settings file and an event subscriber.
        let mut cx = TestAppContext::single();
        let directory = setup(&mut cx);
        let (entity, observed, _subscription) = cx.update(|cx| {
            let entity = cx.global::<GlobalSettings>().0.clone();
            let observed = std::sync::Arc::new(std::sync::Mutex::new(None));
            let output = observed.clone();
            let subscription = cx.subscribe(&entity, move |_, event: &super::SettingsEvent, _| {
                *output.lock().expect("event lock is available") = Some(event.clone());
            });
            (entity, observed, subscription)
        });
        // When changing only the theme.
        cx.update(|cx| {
            entity.update(cx, |store, cx| {
                store.update(cx, |settings| settings.theme.mode = ThemeModeSetting::Dark)
            })
        })
        .expect("settings update succeeds");
        // Then the document is saved and only theme is reported as changed.
        let saved = fs::read_to_string(directory.path().join("settings.json"))
            .expect("settings file is saved");
        assert!(saved.contains("\"mode\": \"dark\""));
        assert_eq!(
            *observed.lock().expect("event lock is available"),
            Some(super::SettingsEvent::Changed {
                theme: true,
                window_mode: false,
            })
        );
    }

    #[gpui_kit::test]
    fn corrupt_file_is_backed_up_and_noticed() {
        // Given a corrupt settings document.
        let cx = TestAppContext::single();
        let directory = tempfile::tempdir().expect("temporary directory is created");
        let path = directory.path().join("settings.json");
        fs::write(&path, b"{ invalid").expect("corrupt file is written");
        let paths = ConfigPaths::at(directory.path().to_path_buf());
        cx.update(|cx| init(cx, &paths));
        // When the load notice is taken.
        let notice = cx.update(|cx| {
            let entity = cx.global::<GlobalSettings>().0.clone();
            entity.update(cx, |store, _| store.take_load_notice())
        });
        // Then defaults are available and original bytes are preserved in a backup.
        let super::LoadNotice::Corrupt { backup } = notice.expect("corruption is noticed") else {
            panic!("expected a corrupt-file notice");
        };
        assert_eq!(fs::read(backup).expect("backup is present"), b"{ invalid");
    }

    #[gpui_kit::test]
    fn newer_version_is_read_only() {
        // Given a settings document written by a newer version.
        let cx = TestAppContext::single();
        let directory = tempfile::tempdir().expect("temporary directory is created");
        let path = directory.path().join("settings.json");
        let original = br#"{"version":99,"future":true}"#;
        fs::write(&path, original).expect("newer settings file is written");
        let paths = ConfigPaths::at(directory.path().to_path_buf());
        cx.update(|cx| init(cx, &paths));
        // When attempting an update.
        let result = cx.update(|cx| {
            let entity = cx.global::<GlobalSettings>().0.clone();
            entity.update(cx, |store, cx| store.update(cx, |_| {}))
        });
        // Then writes are rejected and the original bytes remain untouched.
        assert!(matches!(result, Err(super::SettingsError::ReadOnly)));
        assert_eq!(fs::read(path).expect("settings file remains"), original);
    }

    #[gpui_kit::test]
    fn unknown_fields_are_ignored() {
        // Given settings JSON with an unknown future field.
        let cx = TestAppContext::single();
        let directory = tempfile::tempdir().expect("temporary directory is created");
        fs::write(
            directory.path().join("settings.json"),
            br#"{"version":1,"future":true}"#,
        )
        .expect("settings file is written");
        let paths = ConfigPaths::at(directory.path().to_path_buf());
        cx.update(|cx| init(cx, &paths));
        // When reading settings.
        let settings = cx.update(|cx| cx.global::<GlobalSettings>().0.read(cx).settings().clone());
        // Then known settings load using defaults.
        assert_eq!(settings, Settings::default());
    }

    #[gpui_kit::test]
    fn window_mode_round_trips() {
        // Given a settings file selecting one window per connection.
        let cx = TestAppContext::single();
        let directory = tempfile::tempdir().expect("temporary directory is created");
        fs::write(
            directory.path().join("settings.json"),
            br#"{"version":1,"window_mode":"per_connection"}"#,
        )
        .expect("settings file is written");
        let paths = ConfigPaths::at(directory.path().to_path_buf());
        cx.update(|cx| init(cx, &paths));
        // When reading the selected mode.
        let mode = cx.update(|cx| {
            cx.global::<GlobalSettings>()
                .0
                .read(cx)
                .settings()
                .window_mode
        });
        // Then the serialized mode is parsed as its typed variant.
        assert_eq!(mode, WindowMode::PerConnection);
    }
}
