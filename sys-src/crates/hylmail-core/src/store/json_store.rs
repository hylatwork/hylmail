//! Atomic JSON documents.
//!
//! Every human readable configuration file of HylMail is written through
//! [`write_json_atomic`].  The sequence is
//!
//! 1. create a temporary file *in the destination directory*,
//! 2. write the complete document and `fsync` it,
//! 3. atomically rename it over the destination,
//! 4. `fsync` the directory (unix) so the rename survives a power loss.
//!
//! A crash therefore either leaves the old file or the new file in place -
//! never a half written one.  This is the same guarantee the specification
//! demands for accounts, settings, rules, drafts and the outgoing queue.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::Serialize;
use tempfile::NamedTempFile;

use crate::error::{Error, IoResultExt, Result};
use crate::models::utc_now;

/// Reads and parses a JSON document.
///
/// Returns `Ok(None)` when the file does not exist, which lets callers apply
/// their own default without treating a missing file as an error.
pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(Error::io(path, error)),
    };
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(None);
    }
    let value = serde_json::from_slice(&bytes).map_err(|error| {
        Error::Config(format!("`{}` is not valid JSON: {error}", path.display()))
    })?;
    Ok(Some(value))
}

/// Like [`read_json`] but falls back to `T::default()` for a missing file.
pub fn read_json_or_default<T: DeserializeOwned + Default>(path: &Path) -> Result<T> {
    Ok(read_json::<T>(path)?.unwrap_or_default())
}

/// Serialises `value` and atomically replaces `path`.
pub fn write_json_atomic<T: Serialize + ?Sized>(path: &Path, value: &T) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::Config(format!("`{}` has no parent directory", path.display())))?;
    std::fs::create_dir_all(parent).with_path(parent)?;

    let mut temporary = NamedTempFile::new_in(parent).with_path(parent)?;
    {
        let file = temporary.as_file_mut();
        serde_json::to_writer_pretty(&mut *file, value)?;
        file.write_all(b"\n").with_path(path)?;
        file.flush().with_path(path)?;
        file.sync_all().with_path(path)?;
    }

    temporary
        .persist(path)
        .map_err(|error| Error::io(path, error.error))?;

    sync_directory(parent);
    Ok(())
}

/// Renames a corrupt document out of the way so that the application can start
/// with defaults instead of refusing to boot.
///
/// Returns the path the file was moved to, or `None` when there was nothing to
/// quarantine.
pub fn quarantine_corrupt_file(path: &Path) -> Result<Option<PathBuf>> {
    if !path.is_file() {
        return Ok(None);
    }
    let stamp = utc_now().format("%Y%m%dT%H%M%S");
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "config".to_string());
    let target = path.with_file_name(format!("{file_name}.corrupt-{stamp}"));
    std::fs::rename(path, &target).with_path(path)?;
    Ok(Some(target))
}

/// Best effort `fsync` of a directory.
///
/// On Windows a directory cannot be opened as a file, so the operation is
/// skipped; the previous `sync_all` on the temporary file already guarantees
/// that the data itself is on disk.
fn sync_directory(path: &Path) {
    #[cfg(unix)]
    {
        if let Ok(handle) = std::fs::File::open(path) {
            let _ = handle.sync_all();
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize, Deserialize)]
    struct Document {
        name: String,
        count: u32,
    }

    #[test]
    fn missing_file_reads_as_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.json");
        assert!(read_json::<Document>(&path).unwrap().is_none());
        assert_eq!(
            read_json_or_default::<Document>(&path).unwrap(),
            Document::default()
        );
    }

    #[test]
    fn blank_file_reads_as_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blank.json");
        std::fs::write(&path, "  \n\t ").unwrap();
        assert!(read_json::<Document>(&path).unwrap().is_none());
    }

    #[test]
    fn round_trip_preserves_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("doc.json");
        let document = Document {
            name: "ada".to_string(),
            count: 7,
        };
        write_json_atomic(&path, &document).unwrap();
        assert_eq!(read_json::<Document>(&path).unwrap(), Some(document));
    }

    #[test]
    fn overwriting_is_atomic_and_leaves_no_temporary_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("doc.json");
        for count in 0..5 {
            write_json_atomic(
                &path,
                &Document {
                    name: "x".to_string(),
                    count,
                },
            )
            .unwrap();
        }
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(
            entries.len(),
            1,
            "temporary files were left behind: {entries:?}"
        );
        assert_eq!(read_json::<Document>(&path).unwrap().unwrap().count, 4);
    }

    #[test]
    fn parent_directories_are_created() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a").join("b").join("doc.json");
        write_json_atomic(&path, &Document::default()).unwrap();
        assert!(path.is_file());
    }

    #[test]
    fn corrupt_files_are_reported_and_can_be_quarantined() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("doc.json");
        std::fs::write(&path, "{ this is not json").unwrap();

        let error = read_json::<Document>(&path).unwrap_err();
        assert!(matches!(error, Error::Config(_)));
        assert!(error.to_string().contains("not valid JSON"));

        let quarantined = quarantine_corrupt_file(&path).unwrap().unwrap();
        assert!(quarantined.is_file());
        assert!(!path.exists());
        assert!(quarantine_corrupt_file(&path).unwrap().is_none());
    }

    #[test]
    fn written_documents_are_human_readable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("doc.json");
        write_json_atomic(
            &path,
            &Document {
                name: "ada".to_string(),
                count: 1,
            },
        )
        .unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("\"name\": \"ada\""));
        assert!(raw.ends_with('\n'));
    }
}
