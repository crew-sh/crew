use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use prost::Message;

use crate::lock::FileLock;

/// A finished step branch waiting to land.
#[derive(Clone, PartialEq, Message)]
pub struct Submission {
    #[prost(string, tag = "1")]
    pub step_id: String,
    #[prost(string, tag = "2")]
    pub branch: String,
    #[prost(uint64, tag = "3")]
    pub submitted_ms: u64,
}

/// A worktree handed to an agent; dead once its owner process is gone or the step is done.
#[derive(Clone, PartialEq, Message)]
pub struct WorktreeReg {
    #[prost(string, tag = "1")]
    pub step_id: String,
    #[prost(string, tag = "2")]
    pub path: String,
    #[prost(string, tag = "3")]
    pub branch: String,
    #[prost(uint64, tag = "4")]
    pub owner_pid: u64,
    /// The owner's /proc start time, so a reused pid does not look alive.
    #[prost(uint64, tag = "5")]
    pub owner_start: u64,
    #[prost(uint64, tag = "6")]
    pub created_ms: u64,
    #[prost(bool, tag = "7")]
    pub done: bool,
}

#[derive(Clone, PartialEq, Message)]
pub struct QueueFile {
    #[prost(message, repeated, tag = "1")]
    pub submissions: Vec<Submission>,
    #[prost(message, repeated, tag = "2")]
    pub worktrees: Vec<WorktreeReg>,
}

pub struct Store {
    file: PathBuf,
    lock: PathBuf,
}

impl Store {
    pub fn new(crew_dir: &Path) -> Self {
        Self {
            file: crew_dir.join("queue.pb"),
            lock: crew_dir.join("queue.pb.lock"),
        }
    }

    fn read(&self) -> Result<QueueFile> {
        match std::fs::read(&self.file) {
            Ok(bytes) => QueueFile::decode(bytes.as_slice())
                .with_context(|| format!("decode {}", self.file.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(QueueFile::default()),
            Err(e) => Err(e).with_context(|| format!("read {}", self.file.display())),
        }
    }

    pub fn load(&self) -> Result<QueueFile> {
        let _guard = FileLock::exclusive(&self.lock)?;
        self.read()
    }

    /// Read-modify-write under the state lock; the write is atomic.
    pub fn update<R>(&self, f: impl FnOnce(&mut QueueFile) -> R) -> Result<R> {
        let _guard = FileLock::exclusive(&self.lock)?;
        let mut state = self.read()?;
        let out = f(&mut state);
        let tmp = self.file.with_extension("pb.tmp");
        std::fs::write(&tmp, state.encode_to_vec())?;
        std::fs::rename(&tmp, &self.file)?;
        Ok(out)
    }
}
