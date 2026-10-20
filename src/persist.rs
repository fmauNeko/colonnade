//! Atomic JSON persistence for the config files.

use serde::Serialize;
use serde::de::DeserializeOwned;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// A persisted document with a schema version.
pub trait Versioned {
    const CURRENT_VERSION: u32;
}

/// Result of [`load_json`].
#[derive(Debug)]
pub enum LoadOutcome<T> {
    /// The file does not exist.
    Missing,
    Loaded(T),
    /// The file was unreadable as `T` and moved to `backup`.
    Corrupt {
        backup: PathBuf,
        error: String,
    },
    /// The file was written by a newer version and is left untouched.
    NewerVersion {
        found: u64,
    },
}

/// Loads a JSON document, backing up corrupt files instead of overwriting them.
///
/// # Errors
/// Returns I/O errors from reading or from renaming a corrupt file.
pub fn load_json<T: DeserializeOwned + Versioned>(path: &Path) -> io::Result<LoadOutcome<T>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(LoadOutcome::Missing),
        Err(e) => return Err(e),
    };
    let value = match serde_json::from_slice::<serde_json::Value>(&bytes) {
        Ok(value) => value,
        Err(e) => return back_up(path, e.to_string()),
    };
    if let Some(found) = value.get("version").and_then(serde_json::Value::as_u64)
        && found > u64::from(T::CURRENT_VERSION)
    {
        return Ok(LoadOutcome::NewerVersion { found });
    }
    match serde_json::from_value(value) {
        Ok(doc) => Ok(LoadOutcome::Loaded(doc)),
        Err(e) => back_up(path, e.to_string()),
    }
}

fn back_up<T>(path: &Path, error: String) -> io::Result<LoadOutcome<T>> {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let stem = path
        .file_stem()
        .map_or_else(|| OsString::from("file"), OsString::from);
    let mut attempt = 0_u32;
    loop {
        let mut name = stem.clone();
        name.push(format!(".corrupt-{secs}"));
        if attempt > 0 {
            name.push(format!("-{attempt}"));
        }
        name.push(".json");
        let backup = path.with_file_name(name);
        if !backup.exists() {
            fs::rename(path, &backup)?;
            return Ok(LoadOutcome::Corrupt { backup, error });
        }
        attempt += 1;
    }
}

/// Writes `value` as pretty JSON atomically (sibling `.tmp` file + rename).
///
/// # Errors
/// Returns I/O or serialization errors; on failure the existing file is left
/// as it was.
pub fn save_json<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let mut bytes = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
    bytes.push(b'\n');

    let mut tmp_name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?
        .to_owned();
    tmp_name.push(".tmp");
    let tmp = path.with_file_name(tmp_name);

    let mut file = fs::File::create(&tmp)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);

    rename_over(&tmp, path)?;
    sync_parent(path);
    Ok(())
}

#[cfg(windows)]
fn rename_over(from: &Path, to: &Path) -> io::Result<()> {
    let mut retries = 0;
    loop {
        match fs::rename(from, to) {
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied && retries < 5 => {
                retries += 1;
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            result => return result,
        }
    }
}

#[cfg(not(windows))]
fn rename_over(from: &Path, to: &Path) -> io::Result<()> {
    fs::rename(from, to)
}

#[cfg(unix)]
fn sync_parent(path: &Path) {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty())
        && let Ok(dir) = fs::File::open(parent)
    {
        drop(dir.sync_all());
    }
}

#[cfg(not(unix))]
fn sync_parent(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};
    use std::fs;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Doc {
        version: u32,
        name: String,
    }

    impl Versioned for Doc {
        const CURRENT_VERSION: u32 = 2;
    }

    fn doc() -> Doc {
        Doc {
            version: 2,
            name: "x".into(),
        }
    }

    #[test]
    fn round_trip() {
        // Given a saved document
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("sub").join("doc.json");
        save_json(&path, &doc()).unwrap();
        // When loading it
        let out = load_json::<Doc>(&path).unwrap();
        // Then it comes back equal, pretty-printed with a trailing newline
        assert!(matches!(out, LoadOutcome::Loaded(d) if d == doc()));
        assert!(fs::read_to_string(&path).unwrap().ends_with("}\n"));
    }

    #[test]
    fn missing_file_is_missing() {
        // Given no file
        let tmp = tempfile::tempdir().unwrap();
        // When loading
        let out = load_json::<Doc>(&tmp.path().join("nope.json")).unwrap();
        // Then Missing
        assert!(matches!(out, LoadOutcome::Missing));
    }

    #[test]
    fn corrupt_file_is_backed_up() {
        // Given a file with invalid JSON
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("doc.json");
        fs::write(&path, b"{ not json").unwrap();
        // When loading
        let out = load_json::<Doc>(&path).unwrap();
        // Then the backup holds the original bytes and the original is gone
        let LoadOutcome::Corrupt { backup, error } = out else {
            panic!("expected Corrupt");
        };
        assert_ne!(error, "");
        assert_eq!(fs::read(&backup).unwrap(), b"{ not json");
        assert!(!path.exists());
        let name = backup.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("doc.corrupt-"));
        assert_eq!(backup.extension().and_then(|e| e.to_str()), Some("json"));
    }

    #[test]
    fn wrong_shape_is_corrupt() {
        // Given valid JSON of the wrong shape
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("doc.json");
        fs::write(&path, br#"{"version": 1}"#).unwrap();
        // When loading
        let out = load_json::<Doc>(&path).unwrap();
        // Then it is treated as corrupt
        assert!(matches!(out, LoadOutcome::Corrupt { .. }));
    }

    #[test]
    fn second_corruption_keeps_first_backup() {
        // Given two consecutive corrupt files at the same path
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("doc.json");
        fs::write(&path, b"first").unwrap();
        let LoadOutcome::Corrupt { backup: b1, .. } = load_json::<Doc>(&path).unwrap() else {
            panic!("expected Corrupt");
        };
        fs::write(&path, b"second").unwrap();
        // When loading again
        let LoadOutcome::Corrupt { backup: b2, .. } = load_json::<Doc>(&path).unwrap() else {
            panic!("expected Corrupt");
        };
        // Then both backups exist with their own bytes
        assert_ne!(b1, b2);
        assert_eq!(fs::read(&b1).unwrap(), b"first");
        assert_eq!(fs::read(&b2).unwrap(), b"second");
    }

    #[test]
    fn newer_version_is_reported_and_untouched() {
        // Given a file from a newer version
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("doc.json");
        let bytes = br#"{"version": 99, "future": true}"#;
        fs::write(&path, bytes).unwrap();
        // When loading
        let out = load_json::<Doc>(&path).unwrap();
        // Then NewerVersion and the bytes are unchanged
        assert!(matches!(out, LoadOutcome::NewerVersion { found: 99 }));
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }

    #[test]
    fn current_version_loads() {
        // Given a file exactly at the current version
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("doc.json");
        fs::write(&path, br#"{"version": 2, "name": "y"}"#).unwrap();
        // When loading
        let out = load_json::<Doc>(&path).unwrap();
        // Then it loads
        assert!(matches!(out, LoadOutcome::Loaded(d) if d.name == "y"));
    }

    #[test]
    fn save_leaves_no_tmp_file() {
        // Given a save
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("doc.json");
        // When saving twice
        save_json(&path, &doc()).unwrap();
        save_json(&path, &doc()).unwrap();
        // Then only the target remains
        let names: Vec<_> = fs::read_dir(tmp.path()).unwrap().collect();
        assert_eq!(names.len(), 1);
        assert!(!tmp.path().join("doc.json.tmp").exists());
    }

    #[test]
    fn failed_save_keeps_original() {
        // Given an existing file and a directory squatting on the tmp path
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("doc.json");
        save_json(&path, &doc()).unwrap();
        let original = fs::read(&path).unwrap();
        fs::create_dir(tmp.path().join("doc.json.tmp")).unwrap();
        // When saving a different value
        let other = Doc {
            version: 2,
            name: "other".into(),
        };
        let result = save_json(&path, &other);
        // Then it fails and the original is byte-identical
        assert!(result.is_err());
        assert_eq!(fs::read(&path).unwrap(), original);
    }
}
