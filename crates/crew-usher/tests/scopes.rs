//! Real `systemd-run --user --scope` scopes around trivial commands; skipped when user scopes are unavailable.
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crew_proto::{JobKind, RefusalReason, WaitingOn};
use crew_usher::config::Config;
use crew_usher::server::{IdleSource, Paths, Server};
use crew_usher_client::{Client, ClientError, OwnerArg, RunOptions};

/// Scratch dirs go under `CREW_TEST_TMP` when set (a big disk), else the system temp dir.
fn tempdir(prefix: &str) -> tempfile::TempDir {
    let base = std::env::var_os("CREW_TEST_TMP").map_or_else(std::env::temp_dir, PathBuf::from);
    std::fs::create_dir_all(&base).unwrap();
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(base)
        .unwrap()
}

fn unit_name() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("--unit=crew-test-probe-{}-{nanos}", std::process::id())
}

fn skip(why: &str) {
    assert!(
        std::env::var_os("CREW_REQUIRE_SCOPES").is_none(),
        "required but unavailable: {why}"
    );
    eprintln!("skipped: {why}");
}

fn scopes_available() -> bool {
    Command::new("systemd-run")
        .args(["--user", "--scope", "--quiet", &unit_name(), "--", "true"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

struct Fixture {
    dir: tempfile::TempDir,
    server: Option<Server>,
    client: Client,
}

impl Fixture {
    fn start(edit: impl FnOnce(&mut Config)) -> Option<Self> {
        if !scopes_available() {
            skip("systemd-run --user --scope is unavailable");
            return None;
        }
        let dir = tempdir("crew-it-");
        let mut cfg = Config {
            min_free_mem_gib: 0,
            watched_paths: vec![],
            min_free_disk_gib: Default::default(),
            tmp_root: dir.path().join("jobtmp").display().to_string(),
            ..Config::default()
        };
        edit(&mut cfg);
        let paths = Paths {
            socket: dir.path().join("usher.sock"),
            sizes: dir.path().join("sizes.pb"),
            log: dir.path().join("usher.log"),
        };
        let server = Server::start(cfg, &paths, IdleSource::Off, false).unwrap();
        Some(Self {
            client: Client::new(&paths.socket),
            dir,
            server: Some(server),
        })
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.root().join("usher.log")).unwrap_or_default()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(s) = self.server.take() {
            s.stop();
        }
    }
}

fn opts(kind: JobKind, argv: &[&str]) -> RunOptions {
    RunOptions {
        kind: Some(kind),
        argv: argv.iter().map(|s| (*s).to_string()).collect(),
        strict: true,
        ..RunOptions::default()
    }
}

#[test]
fn exit_codes_pass_through_real_scopes() {
    let Some(f) = Fixture::start(|_| {}) else {
        return;
    };
    assert_eq!(f.client.run(&opts(JobKind::Other, &["true"])).unwrap(), 0);
    assert_eq!(
        f.client
            .run(&opts(JobKind::Other, &["sh", "-c", "exit 3"]))
            .unwrap(),
        3
    );
    let log = f.log();
    assert_eq!(log.matches("GRANT ").count(), 2, "{log}");
    assert!(log.contains("exit=3"), "{log}");
}

#[test]
fn the_job_gets_a_private_tmpdir_that_is_removed_afterwards() {
    let Some(f) = Fixture::start(|_| {}) else {
        return;
    };
    let root = f.root().join("jobtmp");
    let script = format!(
        "case \"$TMPDIR\" in {}/*) test -d \"$TMPDIR\" && exit 0;; esac; exit 9",
        root.display()
    );
    assert_eq!(
        f.client
            .run(&opts(JobKind::Other, &["sh", "-c", &script]))
            .unwrap(),
        0
    );
    assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
}

#[test]
fn a_game_over_the_cap_queues_then_runs_when_the_first_ends() {
    let Some(f) = Fixture::start(|c| c.max_games = 1) else {
        return;
    };
    let first = {
        let c = Client::new(f.client.socket());
        thread::spawn(move || c.run(&opts(JobKind::Game, &["sleep", "1"])).unwrap())
    };
    thread::sleep(Duration::from_millis(300));
    let started = Instant::now();
    let second = {
        let c = Client::new(f.client.socket());
        thread::spawn(move || c.run(&opts(JobKind::Game, &["true"])).unwrap())
    };
    let mut saw_queued = false;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && !saw_queued {
        let top = f.client.top().unwrap();
        saw_queued = top
            .queued
            .iter()
            .any(|j| j.waiting_on == WaitingOn::GameCap as i32)
            && top.running.len() == 1;
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        saw_queued,
        "second game never showed as queued on the game cap"
    );
    assert_eq!((first.join().unwrap(), second.join().unwrap()), (0, 0));
    assert!(
        started.elapsed() >= Duration::from_millis(400),
        "second game did not wait"
    );
    assert!(f.client.top().unwrap().queued.is_empty());
}

#[test]
fn display_is_refused_while_the_owner_is_here_and_allowed_when_away() {
    let Some(f) = Fixture::start(|_| {}) else {
        return;
    };
    f.client.set_owner(OwnerArg::Here).unwrap();
    let mut o = opts(JobKind::Game, &["true"]);
    o.display = true;
    match f.client.run(&o) {
        Err(ClientError::Refused { reason, .. }) => {
            assert_eq!(reason, RefusalReason::DisplayForbidden)
        }
        other => panic!("{other:?}"),
    }
    f.client.set_owner(OwnerArg::Away).unwrap();
    assert_eq!(f.client.run(&o).unwrap(), 0);
}

#[test]
fn a_bench_waits_for_the_owner_to_leave() {
    let Some(f) = Fixture::start(|_| {}) else {
        return;
    };
    f.client.set_owner(OwnerArg::Here).unwrap();
    let bench = {
        let c = Client::new(f.client.socket());
        thread::spawn(move || c.run(&opts(JobKind::Bench, &["true"])).unwrap())
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let top = f.client.top().unwrap();
        if top
            .queued
            .iter()
            .any(|j| j.waiting_on == WaitingOn::OwnerPresent as i32)
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "bench never queued on owner presence"
        );
        thread::sleep(Duration::from_millis(50));
    }
    f.client.set_owner(OwnerArg::Away).unwrap();
    assert_eq!(bench.join().unwrap(), 0);
}

#[test]
fn finished_jobs_leave_nothing_in_top() {
    let Some(f) = Fixture::start(|_| {}) else {
        return;
    };
    assert_eq!(f.client.run(&opts(JobKind::Other, &["true"])).unwrap(), 0);
    let top = f.client.top().unwrap();
    assert!(top.running.is_empty() && top.queued.is_empty());
}

#[test]
fn learned_sizes_reach_the_state_file() {
    let Some(f) = Fixture::start(|_| {}) else {
        return;
    };
    assert_eq!(
        f.client
            .run(&opts(JobKind::Other, &["sleep", "1"]))
            .unwrap(),
        0
    );
    assert!(
        f.root().join("sizes.pb").exists(),
        "no memory sample reached the daemon: {}",
        f.log()
    );
}
