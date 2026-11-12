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

/// No event the tools write comes near this; a length above it is damage, not a cut-off record.
const MAX_RECORD: usize = 64 * 1024;
/// Looking for whole records behind a bad length is quadratic; it is only done on files this small.
const SCAN_LIMIT: usize = 4 << 20;

/// Whether `bytes` is a chain of whole, non-empty records that ends exactly at the end.
fn is_record_chain(mut bytes: &[u8]) -> bool {
    let mut records = 0;
    while !bytes.is_empty() {
        let Some((len, head)) = varint(bytes) else {
            return false;
        };
        let Some(body) = usize::try_from(len)
            .ok()
            .and_then(|len| bytes.get(head..head.checked_add(len)?))
        else {
            return false;
        };
        match Event::decode(body) {
            Ok(Event { body: Some(_) }) => records += 1,
            _ => return false,
        }
        bytes = &bytes[head + body.len()..];
    }
    records > 0
}

/// A cut-off last record is dropped (reported in `truncated_tail`). A length that cannot be a record, or one that
/// hides whole records behind it, is damage in the middle of the file: an error, so nothing after it is lost.

pub fn parse(path: &Path, bytes: &[u8]) -> Result<Parsed, Error> {
    let mut events = Vec::new();
    let mut pos = 0;
    while pos < bytes.len() {
        let rest = &bytes[pos..];
        let partial = |events: Vec<Event>, what: &str| Parsed {
            events,
            valid_len: pos,
            truncated_tail: Some(format!(
                "{}: ignoring {} trailing bytes of a partial record ({what}); a copy of the file is kept when the next write cuts it off",
                path.display(),
                rest.len()
            )),
        };
        let Some((len, head)) = varint(rest) else {
            if rest.len() < 10 {
                return Ok(partial(events, "length cut off"));
            }
            return Err(corrupt(path, pos, "bad length".into()));
        };
        let len = len as usize;
        if rest.len() - head < len {
            let hides_records = bytes.len() <= SCAN_LIMIT
                && (head..rest.len()).any(|at| is_record_chain(&rest[at..]));
            if len > MAX_RECORD || hides_records {
                return Err(corrupt(
                    path,
                    pos,
                    format!(
                        "a record claims {len} bytes but the file has {} left",
                        rest.len() - head
                    ),
                ));
            }
            return Ok(partial(events, "body cut off"));
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
