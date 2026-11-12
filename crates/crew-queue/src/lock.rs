use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::time::{Duration, Instant};

/// An advisory exclusive flock held until dropped.
pub struct FileLock {
    _file: File,
}

fn open(path: &Path) -> io::Result<File> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
}

impl FileLock {
    /// None when someone else holds it.
    pub fn try_exclusive(path: &Path) -> io::Result<Option<Self>> {
        let file = open(path)?;
        // SAFETY: flock on a descriptor we own.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc == 0 {
            return Ok(Some(Self { _file: file }));
        }
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::WouldBlock {
            Ok(None)
        } else {
            Err(err)
        }
    }

    /// Like `try_exclusive`, but tries again for up to `wait` before giving up.
    pub fn try_exclusive_for(path: &Path, wait: Duration) -> io::Result<Option<Self>> {
        let deadline = Instant::now() + wait;
        loop {
            if let Some(lock) = Self::try_exclusive(path)? {
                return Ok(Some(lock));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    pub fn exclusive(path: &Path) -> io::Result<Self> {
        let file = open(path)?;
        // SAFETY: flock on a descriptor we own.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { _file: file })
    }
}
