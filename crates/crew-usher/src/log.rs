use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct Log {
    file: Mutex<Option<File>>,
    echo: bool,
}

impl Log {
    /// A log that cannot open its file still echoes to stderr when asked.
    pub fn open(path: &Path, echo: bool) -> Self {
        if let Some(dir) = path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        let file = OpenOptions::new().create(true).append(true).open(path).ok();
        Self {
            file: Mutex::new(file),
            echo,
        }
    }

    pub fn sink() -> Self {
        Self {
            file: Mutex::new(None),
            echo: false,
        }
    }

    pub fn line(&self, text: &str) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let line = format!("{}.{:03} {text}\n", now.as_secs(), now.subsec_millis());
        if let Some(f) = self
            .file
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_mut()
        {
            let _ = f.write_all(line.as_bytes());
        }
        if self.echo {
            eprint!("{line}");
        }
    }
}
