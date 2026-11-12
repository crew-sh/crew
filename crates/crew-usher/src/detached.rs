//! Detached jobs: records the usher keeps across restarts, and how they are stored and shown.
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use crew_proto::prost::Message;
use crew_proto::{DetachedJob, JobPhase, JobSummary, JobsFile};

const FORMAT_VERSION: u32 = 1;
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

pub fn load(path: &Path) -> Vec<DetachedJob> {
    let Ok(bytes) = fs::read(path) else {
        return Vec::new();
    };
    JobsFile::decode(bytes.as_slice()).map_or_else(|_| Vec::new(), |f| f.jobs)
}

pub fn save(path: &Path, jobs: Vec<DetachedJob>) -> io::Result<()> {
    let file = JobsFile {
        version: FORMAT_VERSION,
        jobs,
    };
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    // The records hold the clients' environments, so only their owner may read the file.
    let tmp = path.with_extension("pb.tmp");
    let mut out = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&tmp)?;
    out.write_all(&file.encode_to_vec())?;
    fs::rename(tmp, path)
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
        assert!(load(&path).is_empty());
        let jobs = vec![
            record("a", JobPhase::Queued, 0),
            record("b", JobPhase::Exited, 5),
        ];
        save(&path, jobs.clone()).unwrap();
        assert_eq!(load(&path), jobs);
        assert!(!dir.path().join("jobs.pb.tmp").exists());
        let mode =
            std::os::unix::fs::PermissionsExt::mode(&fs::metadata(&path).unwrap().permissions());
        assert_eq!(mode & 0o777, 0o600);
        fs::write(&path, b"not a protobuf \xff\xff").unwrap();
        assert!(load(&path).is_empty());
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
