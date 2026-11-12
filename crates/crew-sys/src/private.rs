//! Directories only their owner can enter, for per-job temp space.
use std::fs::{self, DirBuilder};
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::Path;

fn uid() -> u32 {
    unsafe { libc::getuid() }
}

fn refuse(path: &Path, why: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("{}: {why}", path.display()),
    )
}

/// Creates `path` (and missing parents) with mode 0700; refuses a directory, or a parent, that another user owns.
pub fn create_dir(path: &Path) -> io::Result<()> {
    DirBuilder::new().recursive(true).mode(0o700).create(path)?;
    let me = uid();
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        let meta = fs::metadata(parent)?;
        if meta.uid() != me {
            return Err(refuse(parent, "owned by another user"));
        }
    }
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_dir() || meta.uid() != me {
        return Err(refuse(path, "not a directory owned by this user"));
    }
    if meta.permissions().mode() & 0o077 != 0 {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_directory_is_private_whatever_the_umask() {
        let base = tempfile::tempdir().unwrap();
        let dir = base.path().join("root/job");
        create_dir(&dir).unwrap();
        for p in [&dir, dir.parent().unwrap()] {
            assert_eq!(fs::metadata(p).unwrap().permissions().mode() & 0o777, 0o700);
        }
    }

    #[test]
    fn a_loose_existing_directory_is_tightened_and_a_file_is_refused() {
        let base = tempfile::tempdir().unwrap();
        let dir = base.path().join("job");
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o777)).unwrap();
        create_dir(&dir).unwrap();
        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let file = base.path().join("file");
        fs::write(&file, "").unwrap();
        assert!(create_dir(&file).is_err());
    }
}
