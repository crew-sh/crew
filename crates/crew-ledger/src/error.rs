use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{}: {message}", path.display())]
    Toml { path: PathBuf, message: String },
    #[error("{}: corrupt record at byte {offset}: {message}", path.display())]
    Corrupt {
        path: PathBuf,
        offset: u64,
        message: String,
    },
    #[error("unknown step `{0}`")]
    UnknownStep(String),
    #[error("step `{0}` needs an owner approval before it can leave PLANNED")]
    NeedsApproval(String),
    #[error("{0}")]
    Invalid(String),
}

impl Error {
    pub(crate) fn io(path: &Path, source: std::io::Error) -> Self {
        Self::Io {
            path: path.to_path_buf(),
            source,
        }
    }

    pub(crate) fn toml(path: &Path, e: toml::de::Error) -> Self {
        Self::Toml {
            path: path.to_path_buf(),
            message: e.to_string(),
        }
    }
}
