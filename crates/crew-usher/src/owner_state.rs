//! The owner's manual `here`/`away` override, kept across usher restarts with the time it was set.
use std::fs;
use std::io;
use std::path::Path;

use crew_proto::OwnerFile;
use crew_proto::prost::Message;

use crate::state_file::{self, Read, write_atomic};

const FORMAT_VERSION: u32 = 1;

/// A `crew owner here|away` override and when it was set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Manual {
    pub present: bool,
    pub since_ms: u64,
}

impl Manual {
    /// Only `away` expires: a forgotten `here` keeps the machine safe for the owner.
    pub fn expired(&self, now_ms: u64, max_ms: u64) -> bool {
        !self.present && now_ms.saturating_sub(self.since_ms) >= max_ms
    }
}

/// Reads the override; a damaged file is set aside and the owner reads as unset.
pub fn load(path: &Path) -> Read<Manual> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Read::Missing,
        Err(e) => {
            return Read::Unusable {
                why: format!("cannot read it: {e}"),
                to: None,
            };
        }
    };
    let why = match OwnerFile::decode(bytes.as_slice()) {
        Ok(f) if f.version <= FORMAT_VERSION => {
            return Read::Loaded(Manual {
                present: f.present,
                since_ms: f.set_at_unix_ms,
            });
        }
        Ok(f) => format!("written by a newer usher (format {})", f.version),
        Err(e) => format!("not a valid owner file: {e}"),
    };
    Read::Unusable {
        why,
        to: state_file::quarantine(path).ok(),
    }
}

/// Writes the override, or removes the file when there is none.
pub fn store(path: &Path, manual: Option<Manual>) -> io::Result<()> {
    let Some(m) = manual else {
        return match fs::remove_file(path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
    };
    let file = OwnerFile {
        version: FORMAT_VERSION,
        present: m.present,
        set_at_unix_ms: m.since_ms,
    };
    write_atomic(path, &file.encode_to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_override_survives_a_write_and_a_read_and_clearing_removes_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usher-owner.pb");
        assert_eq!(load(&path), Read::Missing);
        let m = Manual {
            present: false,
            since_ms: 1_700_000_000_123,
        };
        store(&path, Some(m)).unwrap();
        assert_eq!(load(&path), Read::Loaded(m));
        store(&path, None).unwrap();
        assert_eq!(load(&path), Read::Missing);
        store(&path, None).unwrap();
    }

    #[test]
    fn a_damaged_file_is_moved_aside() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usher-owner.pb");
        fs::write(&path, [0xff, 0xff, 0xff]).unwrap();
        assert!(matches!(load(&path), Read::Unusable { .. }));
        assert!(!path.exists());
    }

    #[test]
    fn only_an_away_that_outlived_the_maximum_expires() {
        let away = Manual {
            present: false,
            since_ms: 1_000,
        };
        assert!(!away.expired(1_999, 1_000));
        assert!(away.expired(2_000, 1_000));
        let here = Manual {
            present: true,
            ..away
        };
        assert!(!here.expired(u64::MAX, 1_000));
    }
}
