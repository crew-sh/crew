use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    Git {
        dir: PathBuf,
        args: String,
        code: Option<i32>,
        stderr: String,
    },
    Refused(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "io: {e}"),
            Self::Git {
                dir,
                args,
                code,
                stderr,
            } => {
                write!(
                    f,
                    "git {args} in {} failed ({code:?}): {}",
                    dir.display(),
                    stderr.trim()
                )
            }
            Self::Refused(why) => write!(f, "refused: {why}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl Error {
    pub(crate) fn failed(dir: &Path, args: &[&str], out: &Output) -> Self {
        Self::Git {
            dir: dir.to_owned(),
            args: args.join(" "),
            code: out.status.code(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }
}

pub(crate) trait OutputExt {
    fn stdout_trimmed(&self) -> String;
}

impl OutputExt for Output {
    fn stdout_trimmed(&self) -> String {
        String::from_utf8_lossy(&self.stdout).trim_end().to_owned()
    }
}

pub(crate) fn git_out(dir: &Path, args: &[&str]) -> Result<Output> {
    Ok(Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()?)
}

/// Run git, return trimmed stdout, error on non-zero exit.
pub(crate) fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = git_out(dir, args)?;
    if out.status.success() {
        Ok(out.stdout_trimmed())
    } else {
        Err(Error::failed(dir, args, &out))
    }
}
