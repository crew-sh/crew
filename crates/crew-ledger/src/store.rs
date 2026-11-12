//! The ledger file: concatenated length-delimited prost `Event`s.

use std::path::Path;

use crew_proto::prost::Message;

use crate::Error;
use crew_proto::ledger::Event;

pub struct Parsed {
    pub events: Vec<Event>,
    /// Bytes of the file that hold complete records.
    pub valid_len: usize,
    pub truncated_tail: Option<String>,
}

pub fn encode(event: &Event) -> Vec<u8> {
    event.encode_length_delimited_to_vec()
}

fn varint(buf: &[u8]) -> Option<(u64, usize)> {
    let mut value = 0u64;
    for (i, b) in buf.iter().take(10).enumerate() {
        value |= u64::from(b & 0x7f) << (7 * i);
        if b & 0x80 == 0 {
            return Some((value, i + 1));
        }
    }
    None
}

/// A partial last record is dropped (reported in `truncated_tail`); a bad record before it is an error.
pub fn parse(path: &Path, bytes: &[u8]) -> Result<Parsed, Error> {
    let mut events = Vec::new();
    let mut pos = 0;
    while pos < bytes.len() {
        let rest = &bytes[pos..];
        let truncated = |what: &str| Parsed {
            events: Vec::new(),
            valid_len: pos,
            truncated_tail: Some(format!(
                "{}: ignoring {} trailing bytes of a partial record ({what})",
                path.display(),
                rest.len()
            )),
        };
        let Some((len, head)) = varint(rest) else {
            if rest.len() < 10 {
                let mut p = truncated("length cut off");
                p.events = events;
                return Ok(p);
            }
            return Err(corrupt(path, pos, "bad length".into()));
        };
        let len = len as usize;
        if rest.len() - head < len {
            let mut p = truncated("body cut off");
            p.events = events;
            return Ok(p);
        }
        let event = Event::decode(&rest[head..head + len])
            .map_err(|e| corrupt(path, pos, e.to_string()))?;
        events.push(event);
        pos += head + len;
    }
    Ok(Parsed {
        events,
        valid_len: pos,
        truncated_tail: None,
    })
}

fn corrupt(path: &Path, offset: usize, message: String) -> Error {
    Error::Corrupt {
        path: path.to_path_buf(),
        offset: offset as u64,
        message,
    }
}
