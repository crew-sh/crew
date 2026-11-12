//! Real `systemd-run --user --scope` scopes around trivial commands; skipped when user scopes are unavailable.
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crew_proto::{
    JobKind, JobPhase, JobsReply, RefusalReason, WaitCondition, WaitOutcome, WaitReply, WaitingOn,
};
use crew_usher::config::Config;
use crew_usher::server::{IdleSource, Paths, Server};
use crew_usher_client::{Client, ClientError, OwnerArg, RunOptions, WaitSpec};

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
    cfg: Config,
    paths: Paths,
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
        let paths = Paths::in_state(dir.path().join("usher.sock"), dir.path());
        let server = Server::start(cfg.clone(), &paths, IdleSource::Off, false).unwrap();
        Some(Self {
            client: Client::new(&paths.socket),
            dir,
            cfg,
            paths,
            server: Some(server),
        })
    }

    /// Stops the usher and starts a new one on the same files, as a service restart does.
    fn restart(&mut self) {
        self.server.take().unwrap().stop();
        let server = Server::start(self.cfg.clone(), &self.paths, IdleSource::Off, false).unwrap();
        self.server = Some(server);
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

fn learned(f: &Fixture, argv: &[&str]) -> Option<u64> {
    use crew_proto::JobRequest;
    let req = JobRequest {
        kind: JobKind::Other as i32,
        argv: argv.iter().map(|s| (*s).to_string()).collect(),
        project: crew_sys::paths::project_of(&std::env::current_dir().unwrap()),
        ..JobRequest::default()
    };
    match crew_usher::sizes::Sizes::load(&f.paths.sizes, 0) {
        crew_usher::state_file::Read::Loaded(s) => s
            .expected(&crew_usher::sizes::key(&req))
            .map(|l| l.peak_mem_bytes),
        _ => None,
    }
}

#[test]
fn learned_sizes_reach_the_state_file() {
    let Some(f) = Fixture::start(|c| c.learn_min_wall_secs = 1) else {
        return;
    };
    assert_eq!(
        f.client
            .run(&opts(JobKind::Other, &["sleep", "2"]))
            .unwrap(),
        0
    );
    assert!(
        learned(&f, &["sleep", "2"]).is_some(),
        "no memory sample reached the daemon: {}",
        f.log()
    );
}

#[test]
fn page_cache_a_job_writes_is_not_learned_as_its_memory() {
    let Some(f) = Fixture::start(|c| c.learn_min_wall_secs = 1) else {
        return;
    };
    // 300 MiB of file written to disk is charged to the scope as page cache, and shows in memory.peak.
    let script =
        "dd if=/dev/zero of=\"$TMPDIR/blob\" bs=1M count=300 status=none; sleep 2; echo cache-run";
    assert_eq!(
        f.client
            .run(&opts(JobKind::Other, &["sh", "-c", script]))
            .unwrap(),
        0
    );
    let peak = learned(&f, &["sh", "-c", script]).expect("the run was learned");
    assert!(
        peak < 100 << 20,
        "page cache was learned as memory: {} MiB",
        peak >> 20
    );
}

#[test]
fn a_failed_run_teaches_the_usher_nothing() {
    let Some(f) = Fixture::start(|c| c.learn_min_wall_secs = 1) else {
        return;
    };
    assert_eq!(
        f.client
            .run(&opts(JobKind::Other, &["sh", "-c", "sleep 2; exit 3"]))
            .unwrap(),
        3
    );
    assert_eq!(learned(&f, &["sh", "-c", "sleep 2; exit 3"]), None);
}

#[test]
fn a_job_larger_than_the_machine_is_refused_at_once_naming_the_numbers() {
    let Some(f) = Fixture::start(|_| {}) else {
        return;
    };
    let huge = RunOptions {
        mem_bytes: Some(100 << 40),
        ..opts(JobKind::Other, &["true"])
    };
    match f.client.run(&huge) {
        Err(ClientError::Refused { reason, detail }) => {
            assert_eq!(reason, RefusalReason::TooLarge);
            assert!(
                detail.contains("102400.0 GiB") && detail.contains("memory"),
                "{detail}"
            );
        }
        other => panic!("{other:?}"),
    }
    // It never blocked the line: the next job runs.
    assert_eq!(f.client.run(&opts(JobKind::Other, &["true"])).unwrap(), 0);
}

#[test]
fn a_watched_path_that_cannot_be_measured_refuses_builds_naming_it() {
    let Some(f) = Fixture::start(|c| {
        c.watched_paths = vec!["/nonexistent-mount-for-crew-test".into()];
        c.min_free_disk_gib = [("/nonexistent-mount-for-crew-test".to_string(), 1)].into();
    }) else {
        return;
    };
    match f.client.run(&opts(JobKind::Build, &["true"])) {
        Err(ClientError::Refused { reason, detail }) => {
            assert_eq!(reason, RefusalReason::DiskLow);
            assert!(
                detail.contains("nonexistent-mount-for-crew-test"),
                "{detail}"
            );
        }
        other => panic!("{other:?}"),
    }
}

/// A stand-in mbx that records its arguments and runs the command after `--`.
fn fake_mbx(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join("fake-mbx");
    std::fs::write(
        &p,
        format!(
            "#!/bin/sh\necho \"$@\" >> {rec}\nwhile [ \"$1\" != -- ]; do shift; done\nshift\nexec \"$@\"\n",
            rec = dir.join("mbx.args").display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

#[test]
fn with_mbx_reserve_on_games_and_harnesses_run_inside_an_mbx_reserve() {
    let dir = tempdir("crew-it-mbx-");
    let mbx = fake_mbx(dir.path());
    let Some(f) = Fixture::start(|c| {
        c.depot.mbx_reserve = true;
        c.depot.mbx_path = Some(mbx.clone());
        c.depot.reserve_cpus = 1;
        c.kinds.harness.expected_mem_gib = Some(3);
    }) else {
        return;
    };
    assert_eq!(
        f.client
            .run(&opts(JobKind::Harness, &["sh", "-c", "exit 4"]))
            .unwrap(),
        4
    );
    assert_eq!(f.client.run(&opts(JobKind::Other, &["true"])).unwrap(), 0);
    let args = std::fs::read_to_string(dir.path().join("mbx.args")).unwrap();
    let lines: Vec<&str> = args.lines().collect();
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("reserve --cpus 1 --memory 3072MiB -- sh -c exit 4")),
        "{args}"
    );
    // Only games and harnesses reserve, and the standing hold of the pool is the usher's own `reserve ... -- cat`.
    assert!(lines.iter().all(|l| !l.ends_with("-- true")), "{args}");
}

#[test]
fn the_flag_off_leaves_a_harness_unwrapped() {
    let dir = tempdir("crew-it-mbx-");
    let mbx = fake_mbx(dir.path());
    let Some(f) = Fixture::start(|c| c.depot.mbx_path = Some(mbx.clone())) else {
        return;
    };
    assert_eq!(f.client.run(&opts(JobKind::Harness, &["true"])).unwrap(), 0);
    assert!(!dir.path().join("mbx.args").exists());
}

fn detach_opts(kind: JobKind, argv: &[&str], log: Option<&Path>) -> RunOptions {
    RunOptions {
        log: log.map(Path::to_path_buf),
        ..opts(kind, argv)
    }
}

fn wait_for(client: &Client, id: &str, condition: WaitCondition) -> WaitReply {
    client
        .wait(&WaitSpec {
            condition,
            job_id: id.to_string(),
            timeout: Some(Duration::from_secs(30)),
        })
        .unwrap()
}

fn phase_of(jobs: &JobsReply, id: &str) -> Option<JobPhase> {
    jobs.jobs
        .iter()
        .find(|j| j.id == id)
        .map(|j| JobPhase::try_from(j.phase).unwrap())
}

fn until<T>(what: &str, mut probe: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(v) = probe() {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_detached_job_runs_without_its_client_and_wait_returns_its_exit_code() {
    let Some(f) = Fixture::start(|_| {}) else {
        return;
    };
    let log = f.root().join("out/job.log");
    let job = f
        .client
        .detach(&detach_opts(
            JobKind::Other,
            &["sh", "-c", "echo to-out; echo to-err >&2; exit 3"],
            Some(&log),
        ))
        .unwrap();
    assert_eq!(job.log_path, log);
    let w = wait_for(&f.client, &job.job_id, WaitCondition::JobExit);
    assert_eq!(
        (WaitOutcome::try_from(w.outcome).unwrap(), w.exit_code),
        (WaitOutcome::Satisfied, Some(3)),
        "{}",
        f.log()
    );
    let text = std::fs::read_to_string(&log).unwrap();
    assert!(text.contains("to-out") && text.contains("to-err"), "{text}");
    let jobs = f.client.jobs().unwrap();
    let j = jobs.jobs.iter().find(|j| j.id == job.job_id).unwrap();
    assert_eq!(
        (
            JobPhase::try_from(j.phase).unwrap(),
            j.exit_code,
            j.detached
        ),
        (JobPhase::Exited, Some(3), true)
    );
    assert_eq!(j.log_path, log.display().to_string());
    // Nothing of it is left in the live queue.
    let top = f.client.top().unwrap();
    assert!(top.running.is_empty() && top.queued.is_empty());
}

#[test]
fn a_detached_job_with_the_default_log_runs_in_its_cwd_with_the_clients_environment() {
    let Some(f) = Fixture::start(|_| {}) else {
        return;
    };
    // The test process has HOME and PATH; the job must see this one too.
    let job = f
        .client
        .detach(&detach_opts(
            JobKind::Other,
            &[
                "sh",
                "-c",
                "pwd; echo path-set=${PATH:+yes}; echo job=${CREW_JOB:+yes}",
            ],
            None,
        ))
        .unwrap();
    assert!(job.log_path.starts_with(f.root().join("jobs")), "{job:?}");
    assert_eq!(
        wait_for(&f.client, &job.job_id, WaitCondition::JobExit).exit_code,
        Some(0)
    );
    let text = std::fs::read_to_string(&job.log_path).unwrap();
    let cwd = std::env::current_dir().unwrap();
    assert!(text.contains(&cwd.display().to_string()), "{text}");
    assert!(
        text.contains("path-set=yes") && text.contains("job=yes"),
        "{text}"
    );
}

#[test]
fn queued_and_running_detached_jobs_survive_a_daemon_restart() {
    let Some(mut f) = Fixture::start(|_| {}) else {
        return;
    };
    f.client.set_owner(OwnerArg::Here).unwrap();
    let order = f.root().join("order");
    let append = |name: &str| format!("echo {name} >> {}", order.display());
    let running = f
        .client
        .detach(&detach_opts(
            JobKind::Other,
            &["sh", "-c", "sleep 4; exit 5"],
            None,
        ))
        .unwrap();
    // Two benches wait for the owner to leave; they must keep this order across the restart.
    let first = f
        .client
        .detach(&detach_opts(
            JobKind::Bench,
            &["sh", "-c", &append("first")],
            None,
        ))
        .unwrap();
    let second = f
        .client
        .detach(&detach_opts(
            JobKind::Bench,
            &["sh", "-c", &append("second")],
            None,
        ))
        .unwrap();
    until("the long job to run", || {
        (phase_of(&f.client.jobs().unwrap(), &running.job_id) == Some(JobPhase::Running))
            .then_some(())
    });

    f.restart();

    let jobs = f.client.jobs().unwrap();
    let queued: Vec<&str> = jobs
        .jobs
        .iter()
        .filter(|j| j.phase == JobPhase::Queued as i32)
        .map(|j| j.id.as_str())
        .collect();
    assert_eq!(queued, [first.job_id.as_str(), second.job_id.as_str()]);
    assert_eq!(
        phase_of(&jobs, &running.job_id),
        Some(JobPhase::Running),
        "{}",
        f.log()
    );
    assert!(
        f.log().contains("RESTORE queued=2 adopted=1"),
        "{}",
        f.log()
    );
    assert!(!order.exists(), "a bench ran while the owner was here");

    // The adopted job's exit status comes from the wrapper's status file.
    let w = wait_for(&f.client, &running.job_id, WaitCondition::JobExit);
    assert_eq!(w.exit_code, Some(5), "{}", f.log());
    f.client.set_owner(OwnerArg::Away).unwrap();
    for id in [&first.job_id, &second.job_id] {
        assert_eq!(
            wait_for(&f.client, id, WaitCondition::JobExit).exit_code,
            Some(0)
        );
    }
    assert_eq!(std::fs::read_to_string(&order).unwrap(), "first\nsecond\n");
}

#[test]
fn a_job_that_ended_while_the_daemon_was_down_keeps_its_exit_status() {
    let Some(mut f) = Fixture::start(|_| {}) else {
        return;
    };
    let job = f
        .client
        .detach(&detach_opts(
            JobKind::Other,
            &["sh", "-c", "sleep 1; exit 7"],
            None,
        ))
        .unwrap();
    until("the job to run", || {
        (phase_of(&f.client.jobs().unwrap(), &job.job_id) == Some(JobPhase::Running)).then_some(())
    });
    f.server.take().unwrap().stop();
    thread::sleep(Duration::from_millis(2500));
    f.server = Some(Server::start(f.cfg.clone(), &f.paths, IdleSource::Off, false).unwrap());
    let w = wait_for(&f.client, &job.job_id, WaitCondition::JobExit);
    assert_eq!(w.exit_code, Some(7), "{}", f.log());
}

#[test]
fn a_queued_client_outlives_a_daemon_restart_and_never_runs_unmanaged() {
    let Some(mut f) = Fixture::start(|_| {}) else {
        return;
    };
    f.client.set_owner(OwnerArg::Here).unwrap();
    let marker = f.root().join("ran");
    let client = Client::new(f.client.socket());
    // Not strict: a queued job must still not fall back to running by itself.
    let o = RunOptions {
        strict: false,
        ..opts(
            JobKind::Bench,
            &["sh", "-c", &format!("touch {}", marker.display())],
        )
    };
    let bench = thread::spawn(move || client.run(&o));
    until("the bench to queue on the owner", || {
        f.client
            .top()
            .unwrap()
            .queued
            .iter()
            .any(|j| j.waiting_on == WaitingOn::OwnerPresent as i32)
            .then_some(())
    });
    thread::sleep(Duration::from_millis(1500));

    f.restart();

    let queued = until("the client to queue again", || {
        f.client.top().unwrap().queued.pop()
    });
    assert_eq!(queued.waiting_on, WaitingOn::OwnerPresent as i32);
    assert!(
        queued.age_ms >= 1500,
        "queue time was lost: {}ms",
        queued.age_ms
    );
    thread::sleep(Duration::from_millis(500));
    assert!(!marker.exists(), "the bench ran while the owner was here");
    f.client.set_owner(OwnerArg::Away).unwrap();
    assert_eq!(bench.join().unwrap().unwrap(), 0);
    assert!(marker.exists());
    assert!(f.log().contains("RESUBMIT"), "{}", f.log());
}

#[test]
fn cancel_removes_a_queued_job_and_stops_a_running_one() {
    let Some(f) = Fixture::start(|_| {}) else {
        return;
    };
    f.client.set_owner(OwnerArg::Here).unwrap();
    let queued = f
        .client
        .detach(&detach_opts(JobKind::Bench, &["true"], None))
        .unwrap();
    let running = f
        .client
        .detach(&detach_opts(JobKind::Other, &["sleep", "60"], None))
        .unwrap();
    until("the sleep to run", || {
        (phase_of(&f.client.jobs().unwrap(), &running.job_id) == Some(JobPhase::Running))
            .then_some(())
    });

    assert!(f.client.cancel(&queued.job_id).unwrap().ok);
    let w = wait_for(&f.client, &queued.job_id, WaitCondition::JobExit);
    assert_eq!(
        WaitOutcome::try_from(w.outcome).unwrap(),
        WaitOutcome::NeverGranted
    );

    assert!(f.client.cancel(&running.job_id).unwrap().ok);
    let w = wait_for(&f.client, &running.job_id, WaitCondition::JobExit);
    assert_eq!(
        WaitOutcome::try_from(w.outcome).unwrap(),
        WaitOutcome::JobLost
    );
    let jobs = f.client.jobs().unwrap();
    assert_eq!(phase_of(&jobs, &queued.job_id), Some(JobPhase::Cancelled));
    assert_eq!(phase_of(&jobs, &running.job_id), Some(JobPhase::Cancelled));
    assert!(f.client.top().unwrap().running.is_empty());
    assert!(!f.client.cancel("no-such-job").unwrap().ok);
}

#[test]
fn cancelling_a_queued_client_refuses_it() {
    let Some(f) = Fixture::start(|_| {}) else {
        return;
    };
    f.client.set_owner(OwnerArg::Here).unwrap();
    let client = Client::new(f.client.socket());
    let bench = thread::spawn(move || client.run(&opts(JobKind::Bench, &["true"])));
    let id = until("the bench to queue", || {
        f.client.top().unwrap().queued.pop().map(|j| j.id)
    });
    assert!(f.client.cancel(&id).unwrap().ok);
    match bench.join().unwrap() {
        Err(ClientError::Refused { reason, .. }) => assert_eq!(reason, RefusalReason::Cancelled),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_stale_connection_closing_does_not_drop_the_job_its_client_queued_again() {
    let Some(f) = Fixture::start(|_| {}) else {
        return;
    };
    f.client.set_owner(OwnerArg::Here).unwrap();
    // Two clients with the same job id, as a reconnect that beats the old socket's close looks.
    let req = |id: &str| crew_proto::JobRequest {
        id: id.into(),
        kind: JobKind::Bench as i32,
        argv: vec!["true".into()],
        cwd: "/".into(),
        ..crew_proto::JobRequest::default()
    };
    let send = |r: crew_proto::JobRequest| {
        let mut s = std::os::unix::net::UnixStream::connect(f.client.socket()).unwrap();
        crew_proto::write_frame(
            &mut s,
            &crew_proto::Envelope::of(crew_proto::envelope::Body::JobRequest(r)),
        )
        .unwrap();
        s
    };
    let old = send(req("same"));
    until("the job to queue", || f.client.top().unwrap().queued.pop());
    let new = send(req("same"));
    thread::sleep(Duration::from_millis(300));
    drop(old);
    thread::sleep(Duration::from_millis(500));
    let top = f.client.top().unwrap();
    assert_eq!(
        top.queued.len(),
        1,
        "the old connection's close removed the job"
    );
    drop(new);
}

/// Cancels a detached job when a failing test unwinds, so no scope outlives the run.
struct StopJob(Client, String);

impl Drop for StopJob {
    fn drop(&mut self) {
        let _ = self.0.cancel(&self.1);
    }
}

#[test]
fn the_tick_runs_while_messages_keep_arriving() {
    let Some(f) = Fixture::start(|_| {}) else {
        return;
    };
    let busy = f
        .client
        .detach(&detach_opts(JobKind::Other, &["sleep", "300"], None))
        .unwrap();
    let _stop_it = StopJob(Client::new(f.client.socket()), busy.job_id.clone());
    until("the busy job to run", || {
        (phase_of(&f.client.jobs().unwrap(), &busy.job_id) == Some(JobPhase::Running)).then_some(())
    });
    // A message every 100 ms never lets a 2 s receive timeout expire; only the tick reads a job's usage.
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flood = {
        let (stop, client) = (stop.clone(), Client::new(f.client.socket()));
        thread::spawn(move || {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let _ = client.top();
                thread::sleep(Duration::from_millis(100));
            }
        })
    };
    let mem = until("the tick to read the job's memory", || {
        let top = f.client.top().unwrap();
        top.running.first().map(|j| j.mem_bytes).filter(|m| *m > 0)
    });
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    flood.join().unwrap();
    assert!(mem > 0);
}

/// Sends one raw frame on a new connection, the way a client that is not `crew` would.
fn raw(f: &Fixture, body: crew_proto::envelope::Body) -> std::os::unix::net::UnixStream {
    let mut s = std::os::unix::net::UnixStream::connect(f.client.socket()).unwrap();
    crew_proto::write_frame(&mut s, &crew_proto::Envelope::of(body)).unwrap();
    s
}

fn raw_ack(s: &mut std::os::unix::net::UnixStream) -> crew_proto::Ack {
    match crew_proto::read_frame(s).unwrap().unwrap().body {
        Some(crew_proto::envelope::Body::Ack(a)) => a,
        other => panic!("{other:?}"),
    }
}

fn job_request(id: &str, kind: JobKind, argv: &[&str]) -> crew_proto::JobRequest {
    crew_proto::JobRequest {
        id: id.into(),
        kind: kind as i32,
        argv: argv.iter().map(|s| (*s).to_string()).collect(),
        cwd: "/".into(),
        ..crew_proto::JobRequest::default()
    }
}

/// Stops a scope when a failing test unwinds.
struct StopScope(String);

impl Drop for StopScope {
    fn drop(&mut self) {
        crew_sys::scope::stop(&self.0);
    }
}

#[test]
fn a_restarted_usher_still_counts_the_crew_run_jobs_that_are_running() {
    let Some(mut f) = Fixture::start(|_| {}) else {
        return;
    };
    f.client.set_owner(OwnerArg::Away).unwrap();
    let client = Client::new(f.client.socket());
    let run = thread::spawn(move || client.run(&opts(JobKind::Other, &["sleep", "4"])));
    let id = until("the job to run", || {
        f.client
            .top()
            .unwrap()
            .running
            .first()
            .map(|j| j.id.clone())
    });
    let _stop = StopScope(format!("crew-other-{id}"));

    f.restart();

    let top = f.client.top().unwrap();
    assert_eq!(
        top.running.len(),
        1,
        "the usher forgot a running job: {}",
        f.log()
    );
    assert_eq!(top.running[0].id, id);
    assert!(
        f.log().contains("RESTORE") && f.log().contains("adopted=1"),
        "{}",
        f.log()
    );
    // An exclusive bench needs the machine to itself, so it must wait for the job the old usher granted.
    f.client.set_owner(OwnerArg::Away).unwrap();
    let bench = f
        .client
        .detach(&RunOptions {
            exclusive: true,
            ..detach_opts(JobKind::Bench, &["true"], None)
        })
        .unwrap();
    thread::sleep(Duration::from_millis(1500));
    assert_eq!(
        phase_of(&f.client.jobs().unwrap(), &bench.job_id),
        Some(JobPhase::Queued),
        "{}",
        f.log()
    );
    assert_eq!(run.join().unwrap().unwrap(), 0);
    let w = wait_for(&f.client, &bench.job_id, WaitCondition::JobExit);
    assert_eq!(w.exit_code, Some(0), "{w:?}\n{}", f.log());
}

#[test]
fn an_adopted_job_whose_client_died_while_no_usher_watched_is_stopped() {
    let Some(mut f) = Fixture::start(|_| {}) else {
        return;
    };
    // The state a dead usher left: a running `crew run` job whose client is a process we control.
    let mut client_proc = Command::new("sleep").arg("60").spawn().unwrap();
    let ticks = crew_sys::procs::read_start_ticks(Path::new("/proc"), client_proc.id()).unwrap();
    let scope = "crew-other-orphan-1";
    let _stop = StopScope(scope.to_string());
    let mut job = Command::new("systemd-run")
        .args([
            "--user",
            "--scope",
            "--quiet",
            &format!("--unit={scope}"),
            "--",
            "sleep",
            "60",
        ])
        .spawn()
        .unwrap();
    until("the scope", || crew_sys::scope::alive(scope).then_some(()));
    f.server.take().unwrap().stop();
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let rec = crew_proto::DetachedJob {
        request: Some(job_request("orphan-1", JobKind::Other, &["sleep", "60"])),
        phase: JobPhase::Running as i32,
        queued_at_unix_ms: now_ms,
        started_at_unix_ms: now_ms,
        scope_name: scope.to_string(),
        attached: true,
        client_pid: client_proc.id(),
        client_start_ticks: ticks,
        ..Default::default()
    };
    crew_usher::detached::save(&f.paths.jobs, vec![rec]).unwrap();
    f.server = Some(Server::start(f.cfg.clone(), &f.paths, IdleSource::Off, false).unwrap());
    assert_eq!(f.client.top().unwrap().running.len(), 1, "{}", f.log());
    thread::sleep(Duration::from_millis(2500));
    assert!(
        crew_sys::scope::alive(scope),
        "the client is alive, so the job stays: {}",
        f.log()
    );

    client_proc.kill().unwrap();
    client_proc.wait().unwrap();
    until("the orphaned scope to be stopped", || {
        (!crew_sys::scope::alive(scope)).then_some(())
    });
    until("the reservation to be released", || {
        f.client.top().unwrap().running.is_empty().then_some(())
    });
    let _ = job.wait();
}

#[test]
fn a_command_that_leaves_a_process_behind_does_not_keep_its_scope() {
    let Some(f) = Fixture::start(|_| {}) else {
        return;
    };
    let job = f
        .client
        .detach(&detach_opts(
            JobKind::Other,
            &["sh", "-c", "sleep 60 & exit 0"],
            None,
        ))
        .unwrap();
    let scope = format!("crew-other-{}", job.job_id);
    let _stop = StopScope(scope.clone());
    assert_eq!(
        wait_for(&f.client, &job.job_id, WaitCondition::JobExit).exit_code,
        Some(0)
    );
    until("the leftover process to be stopped", || {
        (!crew_sys::scope::alive(&scope)).then_some(())
    });
}

#[test]
fn a_failed_systemd_run_is_not_recorded_as_the_commands_exit_code() {
    let Some(f) = Fixture::start(|_| {}) else {
        return;
    };
    // A scope already holding the unit name makes systemd-run fail before it runs anything.
    let id = "collide-1";
    let name = format!("crew-other-{id}");
    let mut squatter = Command::new("systemd-run")
        .args([
            "--user",
            "--scope",
            "--quiet",
            &format!("--unit={name}"),
            "--",
            "sleep",
            "30",
        ])
        .spawn()
        .unwrap();
    let _stop = StopScope(name.clone());
    until("the squatter", || {
        crew_sys::scope::alive(&name).then_some(())
    });
    let mut req = job_request(id, JobKind::Other, &["true"]);
    req.detach = true;
    let mut s = raw(&f, crew_proto::envelope::Body::JobRequest(req));
    let _ = crew_proto::read_frame(&mut s);
    let w = wait_for(&f.client, id, WaitCondition::JobExit);
    let _ = squatter.kill();
    let _ = squatter.wait();
    assert_ne!(
        w.exit_code,
        Some(1),
        "the usher blamed the command: {}",
        f.log()
    );
    assert_eq!(w.exit_code, None, "{}", f.log());
    let jobs = f.client.jobs().unwrap();
    let j = jobs.jobs.iter().find(|j| j.id == id).unwrap();
    assert_eq!(JobPhase::try_from(j.phase).unwrap(), JobPhase::Failed);
    assert!(j.reason.contains("systemd-run"), "{}", j.reason);
}

#[test]
fn only_the_jobs_own_connection_can_end_it() {
    let Some(f) = Fixture::start(|_| {}) else {
        return;
    };
    let client = Client::new(f.client.socket());
    let run = thread::spawn(move || client.run(&opts(JobKind::Other, &["sleep", "3"])));
    let id = until("the job to run", || {
        f.client
            .top()
            .unwrap()
            .running
            .first()
            .map(|j| j.id.clone())
    });
    let done = crew_proto::JobDone {
        job_id: id.clone(),
        exit_code: 0,
        wall_ms: 3_600_000,
        peak_mem_bytes: 1 << 20,
        ..Default::default()
    };
    let _forged = raw(&f, crew_proto::envelope::Body::JobDone(done));
    thread::sleep(Duration::from_millis(500));
    assert_eq!(
        f.client.top().unwrap().running.len(),
        1,
        "a stranger's JobDone released the reservation"
    );
    assert!(f.log().contains("IGNORE JobDone"), "{}", f.log());
    assert_eq!(run.join().unwrap().unwrap(), 0);
}

#[test]
fn a_connection_that_queued_two_jobs_loses_both_when_it_closes() {
    let Some(f) = Fixture::start(|_| {}) else {
        return;
    };
    f.client.set_owner(OwnerArg::Here).unwrap();
    let mut s = std::os::unix::net::UnixStream::connect(f.client.socket()).unwrap();
    for id in ["multi-1", "multi-2"] {
        let env = crew_proto::Envelope::of(crew_proto::envelope::Body::JobRequest(job_request(
            id,
            JobKind::Bench,
            &["true"],
        )));
        crew_proto::write_frame(&mut s, &env).unwrap();
    }
    until("both to queue", || {
        (f.client.top().unwrap().queued.len() == 2).then_some(())
    });
    drop(s);
    until("both to be dropped", || {
        f.client.top().unwrap().queued.is_empty().then_some(())
    });
}

#[test]
fn a_request_cannot_take_over_a_queued_detached_job_or_claim_queue_time() {
    let Some(f) = Fixture::start(|_| {}) else {
        return;
    };
    f.client.set_owner(OwnerArg::Here).unwrap();
    let queued = f
        .client
        .detach(&detach_opts(JobKind::Bench, &["true"], None))
        .unwrap();
    let mut s = raw(
        &f,
        crew_proto::envelope::Body::JobRequest(job_request(
            &queued.job_id,
            JobKind::Bench,
            &["true"],
        )),
    );
    assert!(!raw_ack(&mut s).ok);
    let jobs = f.client.jobs().unwrap();
    assert!(
        jobs.jobs
            .iter()
            .find(|j| j.id == queued.job_id)
            .unwrap()
            .detached
    );

    let mut claim = job_request("claims-an-hour", JobKind::Bench, &["true"]);
    claim.queued_at_unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        - 3_600_000;
    let _held = raw(&f, crew_proto::envelope::Body::JobRequest(claim));
    let age = until("the claimant to queue", || {
        f.client
            .top()
            .unwrap()
            .queued
            .iter()
            .find(|j| j.id == "claims-an-hour")
            .map(|j| j.age_ms)
    });
    assert!(age < 60_000, "the claimed queue time was believed: {age}ms");
}

#[test]
fn ids_that_could_escape_their_directory_or_collide_are_rejected() {
    let Some(f) = Fixture::start(|_| {}) else {
        return;
    };
    for id in ["", "../escape", "a/b", &"x".repeat(65)] {
        let mut s = raw(
            &f,
            crew_proto::envelope::Body::JobRequest(job_request(id, JobKind::Other, &["true"])),
        );
        assert!(!raw_ack(&mut s).ok, "{id:?}");
    }
    assert!(f.client.top().unwrap().running.is_empty());
}

#[test]
fn connections_past_the_cap_are_turned_away_and_the_usher_stays_usable() {
    let Some(f) = Fixture::start(|c| c.max_connections = 3) else {
        return;
    };
    let held: Vec<_> = (0..3)
        .map(|_| std::os::unix::net::UnixStream::connect(f.client.socket()).unwrap())
        .collect();
    // Let the acceptor register all three before the fourth arrives.
    thread::sleep(Duration::from_millis(300));
    match f.client.top() {
        Err(ClientError::Rejected(_) | ClientError::Protocol | ClientError::Closed) => {}
        other => panic!(
            "a fourth connection was served: {:?}",
            other.map(|t| t.running.len())
        ),
    }
    assert!(f.log().contains("refusing a connection"), "{}", f.log());
    drop(held);
    until("a slot to free up", || f.client.top().ok().map(|_| ()));
}

#[test]
fn a_job_temp_dir_is_private_and_a_killed_clients_leftover_is_swept_at_start() {
    let Some(mut f) = Fixture::start(|_| {}) else {
        return;
    };
    let job = f
        .client
        .detach(&detach_opts(
            JobKind::Other,
            &["sh", "-c", "stat -c mode=%a \"$TMPDIR\""],
            None,
        ))
        .unwrap();
    wait_for(&f.client, &job.job_id, WaitCondition::JobExit);
    let text = std::fs::read_to_string(&job.log_path).unwrap();
    assert!(text.contains("mode=700"), "{text}");

    // A client killed with SIGKILL leaves its dir; the next start removes it, and only dirs shaped like job ids.
    let root = PathBuf::from(&f.cfg.tmp_root);
    let stale = root.join("5cbd0f7a-0000-4000-8000-0000000000aa");
    let recent = root.join("5cbd0f7a-0000-4000-8000-0000000000bb");
    let other = root.join("not-a-job");
    for d in [&stale, &recent, &other] {
        std::fs::create_dir_all(d).unwrap();
        std::fs::write(d.join("blob"), "x").unwrap();
    }
    let old = std::time::SystemTime::now() - Duration::from_secs(3600);
    std::fs::File::open(&stale)
        .unwrap()
        .set_modified(old)
        .unwrap();
    std::fs::File::open(&other)
        .unwrap()
        .set_modified(old)
        .unwrap();
    f.restart();
    assert!(!stale.exists(), "the leftover stayed: {}", f.log());
    assert!(
        recent.exists(),
        "a young dir may belong to a job still starting"
    );
    assert!(other.exists(), "only job-id shaped names are swept");
}
