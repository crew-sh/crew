//! Durable, owner-only state files: written through a synced temp file and rename, set aside when unreadable.
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Replaces `path` with `bytes` so a crash leaves either the old file or the new one, never a cut-off mix.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty());
    if let Some(dir) = dir {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("pb.tmp");
    let mut out = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&tmp)?;
    out.write_all(bytes)?;
    out.sync_all()?;
    fs::rename(&tmp, path)?;
    if let Some(dir) = dir {
        File::open(dir)?.sync_all()?;
    }
    Ok(())
}

/// Moves an unreadable file out of the way (`<name>.corrupt-<unix secs>`) so the next save cannot overwrite it.
pub fn quarantine(path: &Path) -> io::Result<PathBuf> {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".corrupt-{secs}"));
    let to = path.with_file_name(name);
    fs::rename(path, &to)?;
    Ok(to)
}

/// What reading a state file found.
#[derive(Debug, PartialEq, Eq)]
pub enum Read<T> {
    /// No file: a first start.
    Missing,
    Loaded(T),
    /// The file could not be used and was moved to `to` (or left in place when the move failed).
    Unusable {
        why: String,
        to: Option<PathBuf>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_write_replaces_the_file_whole_with_owner_only_access() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/state.pb");
        write_atomic(&path, b"one").unwrap();
        write_atomic(&path, b"two").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"two");
        assert!(!path.with_extension("pb.tmp").exists());
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn quarantine_keeps_the_bytes_under_a_new_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.pb");
        fs::write(&path, b"damaged").unwrap();
        let to = quarantine(&path).unwrap();
        assert!(!path.exists());
        assert!(
            to.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("state.pb.corrupt-")
        );
        assert_eq!(fs::read(to).unwrap(), b"damaged");
    }
}
