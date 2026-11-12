use std::io;

use crew_proto::{FrameError, RefusalReason};

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("usher is not running at {socket} ({source})")]
    DaemonDown { socket: String, source: io::Error },
    #[error("usher refused the job ({}): {detail}", reason.name())]
    Refused { reason: RefusalReason, detail: String },
    #[error("usher closed the connection")]
    Closed,
    #[error("unexpected reply from usher")]
    Protocol,
    #[error("{0}")]
    Frame(#[from] FrameError),
    #[error("cannot start `{command}`: {source}")]
    Spawn { command: String, source: io::Error },
    #[error("nothing to run")]
    EmptyCommand,
    #[error("{0}")]
    Io(#[from] io::Error),
}
