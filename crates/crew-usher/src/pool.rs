//! A standing `mbx reserve` while the owner is present, so compilers leave part of their pool free.
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crew_depot_config::Depot;
use crew_proto::{BuildGrant, BuildPriority, JobKind, PoolReserve};

use crate::config::Config;

const MIB_PER_GIB: u64 = 1024;

/// A failed hold is tried again after this long.
const RETRY_AFTER: Duration = Duration::from_secs(5 * 60);
/// Most of a failing reserve's stderr that is kept for the log.
const STDERR_KEEP: usize = 4096;
/// How long to wait for a finished reserve's stderr to drain before logging what arrived.
const STDERR_GRACE: Duration = Duration::from_millis(250);

/// A child's stderr collected on its own thread, so a grandchild holding the pipe cannot block the usher.
struct Stderr {
    text: Arc<Mutex<Vec<u8>>>,
    eof: Receiver<()>,
}

impl Stderr {
    fn collect(mut pipe: impl Read + Send + 'static) -> Self {
        let text = Arc::new(Mutex::new(Vec::new()));
        let (done, eof) = mpsc::channel();
        let sink = Arc::clone(&text);
        std::thread::spawn(move || {
            let mut chunk = [0u8; 1024];
            while let Ok(n @ 1..) = pipe.read(&mut chunk) {
                let mut kept = sink
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let room = STDERR_KEEP.saturating_sub(kept.len());
                kept.extend_from_slice(&chunk[..n.min(room)]);
            }
            let _ = done.send(());
        });
        Self { text, eof }
    }

    /// What arrived so far, after giving the pipe a moment to reach EOF.
    fn first_line(&self) -> String {
        let _ = self.eof.recv_timeout(STDERR_GRACE);
        let kept = self
            .text
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        String::from_utf8_lossy(&kept)
            .trim()
            .lines()
            .next()
            .unwrap_or("no output")
            .to_string()
    }
}

struct Held {
    stderr: Stderr,
    child: Child,
    // Closing this ends the reservation's `cat`, which releases the capacity; it also closes if the usher dies.
    _release: Option<ChildStdin>,
}

#[derive(Default)]
pub struct PoolHold {
    held: Option<Held>,
    releasing: Vec<Child>,
    /// When a failed start may be tried again; a change of presence clears it.
    failed_at: Option<Instant>,
    /// Said once: mbx cannot be found.
    warned_missing: bool,
}

/// The `mbx reserve` command line that holds `cpus` cores and `memory_mib` MiB until stdin closes.
pub fn reserve_command(mbx: &Path, cpus: u32, memory_mib: u64) -> Command {
    let mut c = Command::new(mbx);
    c.arg("reserve")
        .args(["--cpus", &cpus.to_string()])
        .args(["--memory", &format!("{memory_mib}MiB")])
        .args(["--", "cat"]);
    c
}

impl PoolHold {
    /// Holds while `owner_present` and the setting is on; releases otherwise. Returns lines for the usher log.
    pub fn sync(
        &mut self,
        depot: &Depot,
        owner_present: bool,
        mbx: Option<PathBuf>,
    ) -> Vec<String> {
        let want = depot.mbx_reserve && owner_present;
        let mut log = Vec::new();
        if !want {
            self.failed_at = None;
            self.warned_missing = false;
            if let Some(h) = self.held.take() {
                self.releasing.push(h.child);
                log.push("POOL released".to_string());
            }
            return log;
        }
        if self.held.is_some() || self.failed_at.is_some_and(|t| t.elapsed() < RETRY_AFTER) {
            return log;
        }
        let Some(mbx) = mbx else {
            self.failed_at = Some(Instant::now());
            if !self.warned_missing {
                self.warned_missing = true;
                log.push("WARN depot.mbx_reserve is on but mbx was not found".to_string());
            }
            return log;
        };
        let mut cmd = reserve_command(
            &mbx,
            depot.reserve_cpus,
            depot.reserve_memory_gib * MIB_PER_GIB,
        );
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        match cmd.spawn() {
            Ok(mut child) => {
                let release = child.stdin.take();
                let stderr = child
                    .stderr
                    .take()
                    .map_or_else(|| Stderr::collect(std::io::empty()), Stderr::collect);
                self.held = Some(Held {
                    stderr,
                    child,
                    _release: release,
                });
                log.push(format!(
                    "POOL holding cpus={} mem={}GiB",
                    depot.reserve_cpus, depot.reserve_memory_gib
                ));
            }
            Err(e) => {
                self.failed_at = Some(Instant::now());
                log.push(format!("WARN cannot start {}: {e}", mbx.display()));
            }
        }
        log
    }

    /// Reaps released reservations and notices one that ended on its own.
    pub fn poll(&mut self) -> Vec<String> {
        self.releasing
            .retain_mut(|c| c.try_wait().is_ok_and(|s| s.is_none()));
        let mut log = Vec::new();
        let ended = self
            .held
            .as_mut()
            .and_then(|h| h.child.try_wait().ok().flatten());
        if let Some(status) = ended {
            let held = self.held.take().expect("held checked above");
            self.failed_at = Some(Instant::now());
            log.push(format!(
                "WARN mbx reserve ended ({status}); retrying in {}m: {}",
                RETRY_AFTER.as_secs() / 60,
                held.stderr.first_line()
            ));
        }
        log
    }

    pub fn holding(&self) -> bool {
        self.held.is_some()
    }
}

/// What the usher knows about one job it grants.
pub struct Job {
    pub kind: JobKind,
    pub expected_cores: f64,
    pub expected_mem: u64,
    pub owner_present: bool,
}

/// Builds and tests get a compiler budget; games and harnesses reserve pool capacity when `depot.mbx_reserve` is on.
pub fn terms(
    cfg: &Config,
    job: &Job,
    find_mbx: impl FnOnce() -> Option<PathBuf>,
) -> (Option<BuildGrant>, Option<PoolReserve>) {
    match job.kind {
        JobKind::Build | JobKind::Test => {
            // Never below the kind's own expectation: a learned figure from a no-op build must not starve the next cold one.
            let cores = job.expected_cores.max(cfg.kind(job.kind).expected_cores);
            let priority = if job.owner_present {
                BuildPriority::Low
            } else {
                BuildPriority::Normal
            };
            let grant = BuildGrant {
                priority: priority as i32,
                compile_cpus: (cores.ceil() as u32).max(1),
            };
            (Some(grant), None)
        }
        JobKind::Game | JobKind::Harness if cfg.depot.mbx_reserve => {
            let reserve = find_mbx().map(|mbx| PoolReserve {
                mbx_path: mbx.display().to_string(),
                cpus: (job.expected_cores.ceil() as u32).max(1),
                mem_bytes: job.expected_mem,
            });
            (None, reserve)
        }
        _ => (None, None),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, Instant};

    use super::*;

    /// A stand-in for mbx that records its arguments and runs the command after `--`.
    fn fake_mbx(dir: &Path, exit_early: bool) -> PathBuf {
        let p = dir.join("mbx");
        let body = if exit_early {
            "#!/bin/sh\necho 'no such setting' >&2\nexit 7\n".to_string()
        } else {
            format!(
                "#!/bin/sh\necho \"$@\" > {rec}\nwhile [ \"$1\" != -- ]; do shift; done\nshift\nexec \"$@\"\n",
                rec = dir.join("args").display()
            )
        };
        fs::write(&p, body).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    fn depot(reserve: bool) -> Depot {
        Depot {
            mbx_reserve: reserve,
            reserve_cpus: 3,
            reserve_memory_gib: 2,
            ..Depot::default()
        }
    }

    fn wait_for(what: &str, mut ok: impl FnMut() -> bool) {
        let end = Instant::now() + Duration::from_secs(5);
        while !ok() {
            assert!(Instant::now() < end, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn job(kind: JobKind, cores: f64, owner_present: bool) -> Job {
        Job {
            kind,
            expected_cores: cores,
            expected_mem: 6 << 30,
            owner_present,
        }
    }

    fn cfg(reserve: bool) -> Config {
        Config {
            depot: depot(reserve),
            ..Config::default()
        }
    }

    #[test]
    fn a_build_grant_is_low_priority_while_the_owner_is_present() {
        let mbx = || None;
        let (b, r) = terms(&cfg(false), &job(JobKind::Build, 4.0, true), mbx);
        assert_eq!(b.unwrap().priority, BuildPriority::Low as i32);
        assert!(r.is_none());
        let (b, _) = terms(&cfg(false), &job(JobKind::Test, 4.0, false), mbx);
        assert_eq!(b.unwrap().priority, BuildPriority::Normal as i32);
    }

    #[test]
    fn the_compile_budget_never_drops_below_the_kinds_expectation() {
        let budget = |cores: f64| {
            terms(&cfg(false), &job(JobKind::Build, cores, false), || None)
                .0
                .unwrap()
                .compile_cpus
        };
        assert_eq!(budget(0.1), 4);
        assert_eq!(budget(9.2), 10);
        let mut c = cfg(false);
        c.kinds.build.expected_cores = Some(8.0);
        let (b, _) = terms(&c, &job(JobKind::Build, 1.0, false), || None);
        assert_eq!(b.unwrap().compile_cpus, 8);
    }

    #[test]
    fn games_and_harnesses_reserve_only_when_the_flag_is_on() {
        let mbx = || Some(PathBuf::from("/opt/mbx"));
        let (b, r) = terms(&cfg(true), &job(JobKind::Game, 1.0, true), mbx);
        assert!(b.is_none());
        let r = r.unwrap();
        assert_eq!(
            (r.mbx_path.as_str(), r.cpus, r.mem_bytes),
            ("/opt/mbx", 1, 6 << 30)
        );
        assert!(
            terms(&cfg(true), &job(JobKind::Harness, 1.5, true), mbx)
                .1
                .is_some()
        );
        assert_eq!(
            terms(&cfg(false), &job(JobKind::Game, 1.0, true), mbx),
            (None, None)
        );
        assert_eq!(
            terms(&cfg(true), &job(JobKind::Bench, 1.0, true), mbx),
            (None, None)
        );
        assert_eq!(
            terms(&cfg(true), &job(JobKind::Game, 1.0, true), || None),
            (None, None)
        );
    }

    #[test]
    fn the_reserve_command_line_names_cores_memory_and_a_waiting_command() {
        let c = reserve_command(Path::new("/x/mbx"), 4, 8192);
        let args: Vec<_> = c.get_args().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(
            args,
            ["reserve", "--cpus", "4", "--memory", "8192MiB", "--", "cat"]
        );
    }

    #[test]
    fn presence_holds_and_absence_releases() {
        let dir = tempfile::tempdir().unwrap();
        let mbx = fake_mbx(dir.path(), false);
        let mut pool = PoolHold::default();
        let d = depot(true);
        let log = pool.sync(&d, true, Some(mbx.clone()));
        assert_eq!(log, ["POOL holding cpus=3 mem=2GiB"]);
        wait_for("the reserve to start", || dir.path().join("args").exists());
        assert_eq!(
            fs::read_to_string(dir.path().join("args")).unwrap().trim(),
            "reserve --cpus 3 --memory 2048MiB -- cat"
        );
        assert!(pool.holding());
        assert!(
            pool.sync(&d, true, Some(mbx.clone())).is_empty(),
            "no second hold"
        );
        assert_eq!(pool.sync(&d, false, Some(mbx)), ["POOL released"]);
        assert!(!pool.holding());
        wait_for("the released reserve to exit", || {
            pool.poll();
            pool.releasing.is_empty()
        });
    }

    #[test]
    fn the_flag_off_holds_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mbx = fake_mbx(dir.path(), false);
        let mut pool = PoolHold::default();
        assert!(pool.sync(&depot(false), true, Some(mbx)).is_empty());
        assert!(!pool.holding());
        assert!(!dir.path().join("args").exists());
    }

    #[test]
    fn a_reserve_that_fails_is_logged_once_and_not_retried_until_presence_changes() {
        let dir = tempfile::tempdir().unwrap();
        let mbx = fake_mbx(dir.path(), true);
        let mut pool = PoolHold::default();
        let d = depot(true);
        pool.sync(&d, true, Some(mbx.clone()));
        let mut lines = Vec::new();
        wait_for("the failure to show", || {
            lines.extend(pool.poll());
            !lines.is_empty()
        });
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0].contains("no such setting") && lines[0].contains("exit status: 7"),
            "{lines:?}"
        );
        assert!(pool.sync(&d, true, Some(mbx.clone())).is_empty());
        pool.sync(&d, false, Some(mbx.clone()));
        assert!(
            !pool.sync(&d, true, Some(mbx)).is_empty(),
            "retried after a change"
        );
    }

    /// An mbx that fails at once while a child it started keeps the stderr pipe open.
    fn fake_mbx_with_lingering_child(dir: &Path) -> PathBuf {
        let p = dir.join("mbx-linger");
        fs::write(
            &p,
            "#!/bin/sh\n(sleep 20 &)\necho 'the pool is gone' >&2\nexit 9\n",
        )
        .unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    #[test]
    fn a_failed_reserve_whose_child_keeps_stderr_open_does_not_block_the_usher() {
        let dir = tempfile::tempdir().unwrap();
        let mbx = fake_mbx_with_lingering_child(dir.path());
        let mut pool = PoolHold::default();
        pool.sync(&depot(true), true, Some(mbx));
        let started = Instant::now();
        let mut lines = Vec::new();
        wait_for("the failure to show", || {
            lines.extend(pool.poll());
            !lines.is_empty()
        });
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "poll waited on the lingering child"
        );
        assert!(lines[0].contains("the pool is gone"), "{lines:?}");
    }

    #[test]
    fn a_dead_hold_is_tried_again_on_a_timer() {
        let dir = tempfile::tempdir().unwrap();
        let mbx = fake_mbx(dir.path(), true);
        let mut pool = PoolHold::default();
        let d = depot(true);
        pool.sync(&d, true, Some(mbx.clone()));
        wait_for("the failure", || !pool.poll().is_empty());
        assert!(
            pool.sync(&d, true, Some(mbx.clone())).is_empty(),
            "too soon to retry"
        );
        pool.failed_at = Instant::now().checked_sub(RETRY_AFTER + Duration::from_secs(1));
        assert!(
            !pool.sync(&d, true, Some(mbx)).is_empty(),
            "retried after the wait"
        );
    }

    #[test]
    fn a_missing_mbx_warns_once() {
        let mut pool = PoolHold::default();
        let d = depot(true);
        let first = pool.sync(&d, true, None);
        assert!(first[0].contains("mbx was not found"), "{first:?}");
        assert!(pool.sync(&d, true, None).is_empty());
    }
}
