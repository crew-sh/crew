use std::io;

use crew_proto::{FrameError, RefusalReason};

use crate::kind::ProjectRulesError;

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("usher is not running at {socket} ({source})")]
    DaemonDown { socket: String, source: io::Error },
    #[error(
        "lost the usher while job {job_id} was queued, and it did not come back within {secs}s ({source})"
    )]
    UsherLost {
        job_id: String,
        secs: u64,
        source: io::Error,
    },
    #[error("usher rejected the request: {0}")]
    Rejected(String),
    #[error("usher refused the job ({}): {detail}", reason.name())]
    Refused {
        reason: RefusalReason,
        detail: String,
    },
    #[error("usher closed the connection")]
    Closed,
    #[error("unexpected reply from usher")]
    Protocol,
    #[error("{0}")]
    Frame(#[from] FrameError),
    #[error("cannot start `{command}`: {source}")]
    Spawn { command: String, source: io::Error },
    #[error("{0}")]
    ProjectRules(#[from] ProjectRulesError),
    #[error("nothing to run")]
    EmptyCommand,
    #[error("{0}")]
    Io(#[from] io::Error),
}
