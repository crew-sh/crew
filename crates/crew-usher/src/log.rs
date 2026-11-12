use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// A log file is rotated once it would pass this size.
const MAX_BYTES: u64 = 10 << 20;
/// Rotated files kept next to the live one (`usher.log.1` is the newest).
const KEEP_ROTATED: usize = 3;

struct Sink {
    file: File,
    written: u64,
}

pub struct Log {
    path: PathBuf,
    sink: Mutex<Option<Sink>>,
    max_bytes: u64,
    echo: bool,
}

/// Command lines are logged in full, so only the owner reads the file.
fn open_private(path: &Path) -> Option<Sink> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
        .ok()?;
    // A log from an older usher may have been created world-readable.
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    let written = file.metadata().map_or(0, |m| m.len());
    Some(Sink { file, written })
}

fn rotated(path: &Path, n: usize) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{n}"));
    path.with_file_name(name)
}

impl Log {
    /// A log that cannot open its file still echoes to stderr when asked.
    pub fn open(path: &Path, echo: bool) -> Self {
        Self::open_limited(path, echo, MAX_BYTES)
    }

    /// Like `open`, rotating at `max_bytes`.
    pub fn open_limited(path: &Path, echo: bool, max_bytes: u64) -> Self {
        if let Some(dir) = path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        Self {
            path: path.to_path_buf(),
            sink: Mutex::new(open_private(path)),
            max_bytes,
            echo,
        }
    }

    pub fn sink() -> Self {
        Self {
            path: PathBuf::new(),
            sink: Mutex::new(None),
            max_bytes: MAX_BYTES,
            echo: false,
        }
    }

    /// Moves the live file to `.1`, shifting older ones up and dropping the oldest.
    fn rotate(&self) -> Option<Sink> {
        let _ = fs::remove_file(rotated(&self.path, KEEP_ROTATED));
        for n in (1..KEEP_ROTATED).rev() {
            let _ = fs::rename(rotated(&self.path, n), rotated(&self.path, n + 1));
        }
        let _ = fs::rename(&self.path, rotated(&self.path, 1));
        open_private(&self.path)
    }

    pub fn line(&self, text: &str) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let line = format!("{}.{:03} {text}\n", now.as_secs(), now.subsec_millis());
        let mut guard = self
            .sink
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if guard
            .as_ref()
            .is_some_and(|s| s.written > 0 && s.written + line.len() as u64 > self.max_bytes)
        {
            *guard = self.rotate();
        }
        if let Some(s) = guard.as_mut()
            && s.file.write_all(line.as_bytes()).is_ok()
        {
            s.written += line.len() as u64;
        }
        drop(guard);
        if self.echo {
            eprint!("{line}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn a_full_log_moves_aside_and_only_a_few_generations_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usher.log");
        let log = Log::open_limited(&path, false, 200);
        for i in 0..40 {
            log.line(&format!("line number {i:03} of the run"));
        }
        let live = fs::metadata(&path).unwrap().len();
        assert!(live <= 200, "the live log grew to {live}");
        assert!(rotated(&path, 1).exists());
        assert!(!rotated(&path, KEEP_ROTATED + 1).exists());
        // The newest line is in the live file and the file before it ends where this one begins.
        assert!(fs::read_to_string(&path).unwrap().contains("number 039"));
        let before = fs::read_to_string(rotated(&path, 1)).unwrap();
        let first_live = fs::read_to_string(&path)
            .unwrap()
            .lines()
            .next()
            .and_then(|l| l.split("number ").nth(1)?[..3].parse::<u32>().ok())
            .unwrap();
        let last_before = before
            .lines()
            .last()
            .and_then(|l| l.split("number ").nth(1)?[..3].parse::<u32>().ok())
            .unwrap();
        assert_eq!(last_before + 1, first_live, "no line was lost in rotation");
        for n in 1..=KEEP_ROTATED {
            if rotated(&path, n).exists() {
                assert_eq!(mode(&rotated(&path, n)), 0o600);
            }
        }
        assert_eq!(mode(&path), 0o600);
    }

    #[test]
    fn a_log_from_an_older_usher_is_closed_to_other_users() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usher.log");
        fs::write(&path, "old\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        Log::open(&path, false).line("new");
        assert_eq!(mode(&path), 0o600);
        assert!(fs::read_to_string(&path).unwrap().starts_with("old\n"));
    }
}
