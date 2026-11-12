use std::fmt;
use std::io::{self, Read, Write};

use prost::Message;

mod gen_ {
    include!(concat!(env!("OUT_DIR"), "/crew.usher.rs"));
}

pub use gen_::*;

/// Plan and run ledger records (T3).
pub mod ledger {
    include!(concat!(env!("OUT_DIR"), "/crew.ledger.rs"));
}
pub use prost;

pub const MAX_FRAME: usize = 1 << 20;

#[derive(Debug)]
pub enum FrameError {
    Io(io::Error),
    TooLarge(usize),
    Decode(prost::DecodeError),
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "socket: {e}"),
            Self::TooLarge(n) => write!(f, "frame of {n} bytes exceeds {MAX_FRAME}"),
            Self::Decode(e) => write!(f, "bad message: {e}"),
        }
    }
}

impl std::error::Error for FrameError {}

impl From<io::Error> for FrameError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<prost::DecodeError> for FrameError {
    fn from(e: prost::DecodeError) -> Self {
        Self::Decode(e)
    }
}

pub fn write_frame<W: Write>(w: &mut W, env: &Envelope) -> Result<(), FrameError> {
    let len = env.encoded_len();
    if len > MAX_FRAME {
        return Err(FrameError::TooLarge(len));
    }
    let mut buf = Vec::with_capacity(4 + len);
    buf.extend_from_slice(&(len as u32).to_le_bytes());
    env.encode(&mut buf).expect("vec has capacity");
    w.write_all(&buf)?;
    w.flush()?;
    Ok(())
}

/// Returns `None` on a clean EOF between frames.
pub fn read_frame<R: Read>(r: &mut R) -> Result<Option<Envelope>, FrameError> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(FrameError::TooLarge(len));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    Ok(Some(Envelope::decode(body.as_slice())?))
}

impl JobKind {
    pub const ALL: [Self; 7] = [
        Self::Build,
        Self::Test,
        Self::Harness,
        Self::Game,
        Self::Bench,
        Self::Lsp,
        Self::Other,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Unspecified => "unspecified",
            Self::Build => "build",
            Self::Test => "test",
            Self::Harness => "harness",
            Self::Game => "game",
            Self::Bench => "bench",
            Self::Lsp => "lsp",
            Self::Other => "other",
        }
    }

    pub fn from_name(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.name() == s)
    }
}

impl WaitingOn {
    pub fn name(self) -> &'static str {
        match self {
            Self::Unspecified => "-",
            Self::Memory => "memory",
            Self::Cpu => "cpu",
            Self::GameCap => "game cap",
            Self::OwnerPresent => "owner present",
            Self::Disk => "disk",
            Self::Exclusive => "exclusive job",
        }
    }
}

impl RefusalReason {
    pub fn name(self) -> &'static str {
        match self {
            Self::Unspecified => "unspecified",
            Self::DiskLow => "disk low",
            Self::OverGameCap => "game cap is zero",
            Self::DisplayForbidden => "display forbidden",
            Self::UnknownKind => "unknown kind",
        }
    }
}

impl Envelope {
    pub fn of(body: envelope::Body) -> Self {
        Self { body: Some(body) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Envelope {
        Envelope::of(envelope::Body::JobRequest(JobRequest {
            id: "abc".into(),
            kind: JobKind::Build as i32,
            argv: vec!["cargo".into(), "check".into()],
            cwd: "/x".into(),
            project: "p".into(),
            agent: "a".into(),
            declared: Some(Limits {
                mem_bytes: Some(8 << 30),
                cpu_weight: 0,
                io_weight: 5,
                cpus: None,
            }),
            needs_display: true,
            hz_desktop: false,
            display_env: [("DISPLAY".to_string(), ":0".to_string())].into(),
        }))
    }

    #[test]
    fn envelope_round_trips_through_frames() {
        let reply = Envelope::of(envelope::Body::Reply(Reply {
            decision: Decision::Queued as i32,
            outcome: Some(reply::Outcome::Queued(Queued {
                waiting_on: WaitingOn::Memory as i32,
                position: 2,
                queued_ms: 31_000,
                detail: "d".into(),
            })),
        }));
        let mut wire = Vec::new();
        write_frame(&mut wire, &sample()).unwrap();
        write_frame(&mut wire, &reply).unwrap();
        let mut r = wire.as_slice();
        assert_eq!(read_frame(&mut r).unwrap(), Some(sample()));
        assert_eq!(read_frame(&mut r).unwrap(), Some(reply));
        assert_eq!(read_frame(&mut r).unwrap(), None);
    }

    #[test]
    fn oversized_frame_is_rejected() {
        let wire = ((MAX_FRAME as u32) + 1).to_le_bytes();
        assert!(matches!(
            read_frame(&mut wire.as_slice()),
            Err(FrameError::TooLarge(_))
        ));
    }

    #[test]
    fn truncated_frame_is_an_error_not_eof() {
        let mut wire = Vec::new();
        write_frame(&mut wire, &sample()).unwrap();
        wire.truncate(wire.len() - 1);
        assert!(read_frame(&mut wire.as_slice()).is_err());
    }

    #[test]
    fn kind_names_round_trip() {
        for k in JobKind::ALL {
            assert_eq!(JobKind::from_name(k.name()), Some(k));
        }
        assert_eq!(JobKind::from_name("nope"), None);
    }
}
