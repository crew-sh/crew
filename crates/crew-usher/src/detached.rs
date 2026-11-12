//! Detached jobs: records the usher keeps across restarts, and how they are stored and shown.
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crew_proto::prost::Message;
use crew_proto::{DetachedJob, JobPhase, JobSummary, JobsFile};

use crate::state_file::{self, Read};

/// 2: the file also lists attached jobs (running ones, and queued ones that may resubmit). A version-1 file has detached jobs only.
pub const FORMAT_VERSION: u32 = 2;
const KEEP_FINISHED: usize = 100;
const KEEP_FINISHED_MS: u64 = 7 * 24 * 3600 * 1000;

/// Where the usher keeps a detached job's files.
#[derive(Clone, Debug)]
pub struct Dirs {
    pub state: PathBuf,
    pub logs: PathBuf,
}

fn file_stem(id: &str) -> String {
    id.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect()
}

impl Dirs {
    pub fn default_log(&self, id: &str) -> PathBuf {
        self.logs.join(format!("{}.log", file_stem(id)))
    }

    pub fn status(&self, id: &str) -> PathBuf {
        self.logs.join(format!("{}.status", file_stem(id)))
    }

    /// Whether the usher made this log itself, and so may remove it with the job's record.
    fn owns_log(&self, log: &Path) -> bool {
        log.parent() == Some(self.logs.as_path())
    }
}

/// A state file's content with the version of the usher that wrote it.
#[derive(Debug, PartialEq)]
pub struct Stored {
    pub version: u32,
    pub jobs: Vec<DetachedJob>,
}

/// Reads the state file; one that is cut off, damaged or from a newer usher is set aside, never overwritten.
pub fn load(path: &Path) -> Read<Stored> {
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
    let why = match JobsFile::decode(bytes.as_slice()) {
        Ok(f) if f.version <= FORMAT_VERSION => {
            return Read::Loaded(Stored {
                version: f.version,
                jobs: f.jobs,
            });
        }
        Ok(f) => format!(
            "written by a newer usher (format {}, this one reads up to {FORMAT_VERSION})",
            f.version
        ),
        Err(e) => format!("not a valid job state file: {e}"),
    };
    Read::Unusable {
        why,
        to: state_file::quarantine(path).ok(),
    }
}

pub fn save(path: &Path, jobs: Vec<DetachedJob>) -> io::Result<()> {
    let file = JobsFile {
        version: FORMAT_VERSION,
        jobs,
    };
    // The records hold the clients' environments, so only their owner may read the file.
    state_file::write_atomic(path, &file.encode_to_vec())
}

pub fn is_finished(phase: JobPhase) -> bool {
    matches!(
        phase,
        JobPhase::Exited | JobPhase::Failed | JobPhase::Cancelled
    )
}

pub fn phase(job: &DetachedJob) -> JobPhase {
    JobPhase::try_from(job.phase).unwrap_or(JobPhase::Unspecified)
}

/// Drops the oldest finished records past the keep limits and their files; returns how many went.
pub fn prune(finished: &mut Vec<DetachedJob>, now_ms: u64, dirs: &Dirs) -> usize {
    let before = finished.len();
    let excess = finished.len().saturating_sub(KEEP_FINISHED);
    let mut at = 0;
    finished.retain(|j| {
        at += 1;
        let keep = at > excess && now_ms.saturating_sub(j.finished_at_unix_ms) < KEEP_FINISHED_MS;
        if !keep {
            let _ = fs::remove_file(&j.status_path);
            let log = Path::new(&j.log_path);
            if dirs.owns_log(log) {
                let _ = fs::remove_file(log);
            }
        }
        keep
    });
    before - finished.len()
}

pub fn summary(job: &DetachedJob) -> JobSummary {
    let req = job.request.clone().unwrap_or_default();
    JobSummary {
        id: req.id.clone(),
        kind: req.kind,
        agent: req.agent.clone(),
        project: req.project.clone(),
        command: req.argv.join(" "),
        phase: job.phase,
        detached: true,
        exit_code: job.exit_code,
        log_path: job.log_path.clone(),
        reason: job.reason.clone(),
        waiting_on: 0,
        position: 0,
        queued_at_unix_ms: job.queued_at_unix_ms,
        started_at_unix_ms: job.started_at_unix_ms,
        finished_at_unix_ms: job.finished_at_unix_ms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crew_proto::JobRequest;

    fn record(id: &str, phase: JobPhase, finished_at: u64) -> DetachedJob {
        DetachedJob {
            request: Some(JobRequest {
                id: id.into(),
                argv: vec!["true".into()],
                ..JobRequest::default()
            }),
            phase: phase as i32,
            finished_at_unix_ms: finished_at,
            ..DetachedJob::default()
        }
    }

    #[test]
    fn the_state_file_round_trips_through_an_atomic_rename() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jobs.pb");
        assert_eq!(load(&path), Read::Missing);
        let jobs = vec![
            record("a", JobPhase::Queued, 0),
            record("b", JobPhase::Exited, 5),
        ];
        save(&path, jobs.clone()).unwrap();
        assert_eq!(
            load(&path),
            Read::Loaded(Stored {
                version: FORMAT_VERSION,
                jobs
            })
        );
        assert!(!dir.path().join("jobs.pb.tmp").exists());
        let mode =
            std::os::unix::fs::PermissionsExt::mode(&fs::metadata(&path).unwrap().permissions());
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn a_damaged_or_newer_state_file_is_set_aside_not_treated_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jobs.pb");
        let queued = vec![record("a", JobPhase::Queued, 0)];
        save(&path, queued).unwrap();
        // A power cut leaves a prefix of the real file.
        let whole = fs::read(&path).unwrap();
        fs::write(&path, &whole[..whole.len() - 3]).unwrap();
        let Read::Unusable { to: Some(kept), .. } = load(&path) else {
            panic!("a cut-off file must not load");
        };
        assert_eq!(fs::read(&kept).unwrap(), whole[..whole.len() - 3]);
        assert!(!path.exists(), "the next save must not find it in the way");

        let future = JobsFile {
            version: FORMAT_VERSION + 1,
            jobs: vec![],
        };
        fs::write(&path, future.encode_to_vec()).unwrap();
        assert!(matches!(load(&path), Read::Unusable { to: Some(_), .. }));
    }

    #[test]
    fn a_version_one_file_from_the_previous_usher_still_loads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jobs.pb");
        let old = JobsFile {
            version: 1,
            jobs: vec![record("a", JobPhase::Queued, 0)],
        };
        fs::write(&path, old.encode_to_vec()).unwrap();
        let Read::Loaded(stored) = load(&path) else {
            panic!("the previous format must load");
        };
        assert_eq!((stored.version, stored.jobs.len()), (1, 1));
    }

    #[test]
    fn pruning_keeps_the_newest_and_removes_only_the_usher_s_own_files() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = Dirs {
            state: dir.path().to_path_buf(),
            logs: dir.path().join("jobs"),
        };
        fs::create_dir_all(&dirs.logs).unwrap();
        let elsewhere = dir.path().join("mine.log");
        fs::write(&elsewhere, "x").unwrap();
        let mut old = record("old", JobPhase::Exited, 1);
        old.log_path = dirs.default_log("old").display().to_string();
        fs::write(&old.log_path, "x").unwrap();
        let mut custom = record("custom", JobPhase::Exited, 1);
        custom.log_path = elsewhere.display().to_string();
        let mut fresh = record("fresh", JobPhase::Exited, KEEP_FINISHED_MS);
        fresh.log_path = dirs.default_log("fresh").display().to_string();
        fs::write(&fresh.log_path, "x").unwrap();
        let mut all = vec![old, custom, fresh];
        let gone = prune(&mut all, KEEP_FINISHED_MS + 10, &dirs);
        assert_eq!(gone, 2);
        assert_eq!(all.len(), 1);
        assert!(elsewhere.exists(), "a log the user chose is never removed");
        assert!(!dirs.default_log("old").exists());
        assert!(dirs.default_log("fresh").exists());
    }
}
