//! Warm worktrees: build folders of one tree cloned copy-on-write into another, so a new step starts with a warm `target/`.
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

/// `FICLONE` from `<linux/fs.h>`: `_IOW(0x94, 9, int)`.
const FICLONE: u32 = 0x4004_9409;

/// What happened to one folder of the warm list.
#[derive(Debug, PartialEq, Eq)]
pub enum Warmed {
    Cloned {
        files: usize,
    },
    /// The source tree has no such folder.
    NoSource,
    /// The new tree already has it.
    Present,
    /// The filesystem cannot share blocks between files; nothing was copied.
    CannotReflink(String),
    /// Anything else went wrong; nothing was left behind.
    Failed(String),
}

/// A name from `[queue] warm`: a relative path of plain components.
pub fn valid_name(name: &Path) -> bool {
    !name.as_os_str().is_empty() && name.components().all(|c| matches!(c, Component::Normal(_)))
}

/// The errors a filesystem gives when it has no reflinks, or the two files are on different ones.
fn cannot_reflink(e: &io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(libc::EOPNOTSUPP | libc::EXDEV | libc::EINVAL | libc::ENOTTY | libc::ENOSYS)
    )
}

/// One file as a copy-on-write clone, with the source's mode and modification time.
pub fn clone_file(src: &Path, dst: &Path) -> io::Result<()> {
    let meta = fs::metadata(src)?;
    let from = File::open(src)?;
    let to = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(dst)?;
    // SAFETY: both descriptors are open for the call; FICLONE takes the source descriptor.
    let rc = unsafe { libc::ioctl(to.as_raw_fd(), FICLONE as _, from.as_raw_fd()) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    to.set_permissions(fs::Permissions::from_mode(meta.mode() & 0o7777))?;
    to.set_modified(meta.modified()?)
}

/// `src` and everything under it as clones at `dst`, which must not exist. Symlinks are copied as links.
fn clone_tree(src: &Path, dst: &Path, files: &mut usize) -> io::Result<()> {
    fs::create_dir(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let to = dst.join(entry.file_name());
        if kind.is_dir() {
            clone_tree(&entry.path(), &to, files)?;
        } else if kind.is_symlink() {
            std::os::unix::fs::symlink(fs::read_link(entry.path())?, &to)?;
        } else if kind.is_file() {
            clone_file(&entry.path(), &to)?;
            *files += 1;
        }
    }
    let meta = fs::metadata(src)?;
    fs::set_permissions(dst, meta.permissions())?;
    File::open(dst)?.set_modified(meta.modified()?)
}

/// Clones each named folder of `from` into `to` where the filesystem allows it. A folder that cannot be cloned is left out, never copied the slow way.
pub fn warm(from: &Path, to: &Path, names: &[PathBuf]) -> Vec<(PathBuf, Warmed)> {
    names
        .iter()
        .map(|name| (name.clone(), warm_one(from, to, name)))
        .collect()
}

fn warm_one(from: &Path, to: &Path, name: &Path) -> Warmed {
    if !valid_name(name) {
        return Warmed::Failed(format!(
            "`{}` is not a relative folder name",
            name.display()
        ));
    }
    let (src, dst) = (from.join(name), to.join(name));
    // A symlink is never followed out of the tree.
    if !fs::symlink_metadata(&src).is_ok_and(|m| m.is_dir()) {
        return Warmed::NoSource;
    }
    if fs::symlink_metadata(&dst).is_ok() {
        return Warmed::Present;
    }
    if let Some(parent) = dst.parent()
        && let Err(e) = fs::create_dir_all(parent)
    {
        return Warmed::Failed(e.to_string());
    }
    let mut files = 0;
    match clone_tree(&src, &dst, &mut files) {
        Ok(()) => Warmed::Cloned { files },
        Err(e) => {
            let _ = fs::remove_dir_all(&dst);
            if cannot_reflink(&e) {
                Warmed::CannotReflink(e.to_string())
            } else {
                Warmed::Failed(e.to_string())
            }
        }
    }
}

/// One line for a folder that was warmed or could not be; nothing for one that was absent or present.
pub fn note(name: &Path, outcome: &Warmed) -> Option<String> {
    let name = name.display();
    match outcome {
        Warmed::Cloned { files } => Some(format!(
            "queue: warmed {name}/ from the integration worktree ({files} files, copy-on-write)"
        )),
        Warmed::CannotReflink(why) => Some(format!(
            "queue: {name}/ not warmed: this filesystem cannot reflink ({why})"
        )),
        Warmed::Failed(why) => Some(format!("queue: {name}/ not warmed: {why}")),
        Warmed::NoSource | Warmed::Present => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    /// A scratch dir under `CREW_TEST_TMP` (point it at btrfs or XFS to run the cloning tests), else the system temp dir.
    fn scratch() -> tempfile::TempDir {
        let base = std::env::var_os("CREW_TEST_TMP").map_or_else(std::env::temp_dir, PathBuf::from);
        fs::create_dir_all(&base).unwrap();
        tempfile::Builder::new()
            .prefix("crew-warm-")
            .tempdir_in(base)
            .unwrap()
    }

    /// Whether `dir` can hold reflinked files.
    fn can_reflink(dir: &Path) -> bool {
        let (a, b) = (dir.join("probe-a"), dir.join("probe-b"));
        fs::write(&a, "x").unwrap();
        let ok = clone_file(&a, &b).is_ok();
        let _ = fs::remove_file(&a);
        let _ = fs::remove_file(&b);
        ok
    }

    fn tree(root: &Path) {
        fs::create_dir_all(root.join("target/debug/deps")).unwrap();
        fs::write(root.join("target/debug/deps/libx.rlib"), vec![7u8; 100_000]).unwrap();
        fs::write(root.join("target/debug/tool"), "#!/bin/sh\n").unwrap();
        fs::set_permissions(
            root.join("target/debug/tool"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        symlink("deps/libx.rlib", root.join("target/debug/latest")).unwrap();
    }

    #[test]
    fn a_folder_is_cloned_whole_and_the_clone_is_independent_of_the_source() {
        let dir = scratch();
        if !can_reflink(dir.path()) {
            eprintln!(
                "skipped: {} cannot reflink; set CREW_TEST_TMP to a btrfs or XFS dir",
                dir.path().display()
            );
            return;
        }
        let (from, to) = (dir.path().join("a"), dir.path().join("b"));
        fs::create_dir_all(&to).unwrap();
        tree(&from);

        let out = warm(&from, &to, &[PathBuf::from("target")]);
        assert_eq!(out[0].1, Warmed::Cloned { files: 2 });

        let rlib = to.join("target/debug/deps/libx.rlib");
        assert_eq!(fs::read(&rlib).unwrap(), vec![7u8; 100_000]);
        assert_eq!(
            fs::metadata(to.join("target/debug/tool")).unwrap().mode() & 0o777,
            0o755
        );
        assert_eq!(
            fs::read_link(to.join("target/debug/latest")).unwrap(),
            PathBuf::from("deps/libx.rlib")
        );
        assert_eq!(
            fs::metadata(&rlib).unwrap().modified().unwrap(),
            fs::metadata(from.join("target/debug/deps/libx.rlib"))
                .unwrap()
                .modified()
                .unwrap(),
            "cargo decides what is fresh by modification time"
        );
        // Writing to the clone leaves the source alone.
        fs::write(&rlib, "changed").unwrap();
        assert_eq!(
            fs::read(from.join("target/debug/deps/libx.rlib")).unwrap(),
            vec![7u8; 100_000]
        );
    }

    #[test]
    fn a_filesystem_without_reflinks_gets_nothing_not_a_full_copy() {
        // tmpfs has no reflinks; a machine without /dev/shm has nothing to test here.
        let shm = Path::new("/dev/shm");
        if !shm.is_dir() {
            return;
        }
        let dir = tempfile::Builder::new()
            .prefix("crew-warm-")
            .tempdir_in(shm)
            .unwrap();
        if can_reflink(dir.path()) {
            eprintln!("skipped: /dev/shm can reflink here");
            return;
        }
        let (from, to) = (dir.path().join("a"), dir.path().join("b"));
        fs::create_dir_all(&to).unwrap();
        tree(&from);
        let out = warm(&from, &to, &[PathBuf::from("target")]);
        assert!(matches!(out[0].1, Warmed::CannotReflink(_)), "{out:?}");
        assert!(!to.join("target").exists(), "a half copy was left behind");
        assert!(
            note(&out[0].0, &out[0].1)
                .unwrap()
                .contains("cannot reflink")
        );
    }

    #[test]
    fn missing_present_and_unsafe_folders_are_left_alone() {
        let dir = scratch();
        let (from, to) = (dir.path().join("a"), dir.path().join("b"));
        fs::create_dir_all(&to).unwrap();
        tree(&from);
        fs::create_dir_all(to.join("node_modules")).unwrap();
        fs::create_dir_all(from.join("node_modules")).unwrap();
        fs::write(to.join("node_modules/mine"), "keep").unwrap();
        symlink("/etc", from.join(".venv")).unwrap();
        let names = ["nothing", "node_modules", ".venv", "../escape", "/abs"].map(PathBuf::from);
        let out: Vec<Warmed> = warm(&from, &to, &names)
            .into_iter()
            .map(|(_, w)| w)
            .collect();
        assert_eq!(out[0], Warmed::NoSource);
        assert_eq!(out[1], Warmed::Present);
        assert_eq!(
            out[2],
            Warmed::NoSource,
            "a symlink is never followed out of the tree"
        );
        assert!(matches!(out[3], Warmed::Failed(_)) && matches!(out[4], Warmed::Failed(_)));
        assert_eq!(
            fs::read_to_string(to.join("node_modules/mine")).unwrap(),
            "keep"
        );
        assert!(!to.join(".venv").exists());
    }
}
