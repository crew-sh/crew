//! The real `crew` and `crew-usher` binaries talking over a temp socket.
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const CREW: &str = env!("CARGO_BIN_EXE_crew");

fn usher_binary() -> Option<PathBuf> {
    let p = Path::new(CREW).parent()?.join("crew-usher");
    p.exists().then_some(p)
}

fn unit_name() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("--unit=crew-test-probe-cli-{}-{nanos}", std::process::id())
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

struct Daemon {
    child: Child,
    socket: PathBuf,
    dir: tempfile::TempDir,
}

impl Daemon {
    fn start() -> Option<Self> {
        Self::start_with("")
    }

    /// `extra` is appended to the test's usher.toml.
    fn start_with(extra: &str) -> Option<Self> {
        let Some(bin) = usher_binary() else {
            skip("crew-usher is not built (run `cargo build --workspace` first)");
            return None;
        };
        if !scopes_available() {
            skip("systemd-run --user --scope is unavailable");
            return None;
        }
        let dir = tempfile::Builder::new()
            .prefix("crew-cli-")
            .tempdir_in("/projects/.hz-tmp")
            .or_else(|_| tempfile::tempdir())
            .unwrap();
        let socket = dir.path().join("usher.sock");
        let config = dir.path().join("config/crew");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(
            config.join("usher.toml"),
            format!("min_free_mem_gib = 0\nwatched_paths = []\nmin_free_disk_gib = {{}}\ntmp_root = \"{}\"\n{extra}", dir.path().join("jobtmp").display()),
        )
        .unwrap();
        let child = Command::new(bin)
            .env("CREW_SOCKET", &socket)
            .env("XDG_CONFIG_HOME", dir.path().join("config"))
            .env("XDG_STATE_HOME", dir.path().join("state"))
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("CREW_JOB")
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !socket.exists() {
            assert!(Instant::now() < deadline, "daemon never bound its socket");
            std::thread::sleep(Duration::from_millis(20));
        }
        Some(Self { child, socket, dir })
    }

    fn crew(&self, args: &[&str]) -> Output {
        crew_with(&self.socket, args, &[])
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn crew_with(socket: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(CREW);
    cmd.args(args)
        .env("CREW_SOCKET", socket)
        .env_remove("CREW_STRICT")
        .env_remove("CREW_JOB");
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().unwrap()
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn run_passes_exit_codes_and_strips_the_owners_display() {
    let Some(d) = Daemon::start() else { return };
    assert_eq!(d.crew(&["run", "--", "true"]).status.code(), Some(0));
    assert_eq!(
        d.crew(&["run", "--", "sh", "-c", "exit 3"]).status.code(),
        Some(3)
    );
    let probe = crew_with(
        &d.socket,
        &[
            "run",
            "--",
            "sh",
            "-c",
            "echo \"[$WAYLAND_DISPLAY][$DISPLAY]\"",
        ],
        &[("WAYLAND_DISPLAY", "wayland-9"), ("DISPLAY", ":0")],
    );
    assert_eq!(stdout(&probe).trim(), "[][]");
    let private = crew_with(
        &d.socket,
        &["run", "--", "sh", "-c", "echo \"[$DISPLAY]\""],
        &[("DISPLAY", ":99")],
    );
    assert_eq!(stdout(&private).trim(), "[:99]");
}

#[test]
fn top_and_owner_talk_to_the_daemon() {
    let Some(d) = Daemon::start() else { return };
    let top = d.crew(&["top"]);
    assert!(top.status.success());
    let text = stdout(&top);
    assert!(
        text.contains("RUNNING (0)")
            && text.contains("QUEUED (0)")
            && text.contains("owner    present"),
        "{text}"
    );
    assert!(stdout(&d.crew(&["owner", "away"])).contains("away"));
    assert!(stdout(&d.crew(&["top"])).contains("owner    away (manual)"));
    assert!(stdout(&d.crew(&["owner", "auto"])).contains("present"));
}

#[test]
fn display_flag_is_refused_with_a_reason_while_the_owner_is_present() {
    let Some(d) = Daemon::start() else { return };
    let o = d.crew(&["run", "--display", "--kind", "game", "--", "true"]);
    assert_eq!(o.status.code(), Some(75));
    assert!(stderr(&o).contains("display forbidden"), "{}", stderr(&o));
}

#[test]
fn without_a_daemon_the_command_still_runs_unless_strict() {
    let socket = std::env::temp_dir().join("crew-no-such-usher.sock");
    let o = crew_with(&socket, &["run", "--", "sh", "-c", "exit 4"], &[]);
    assert_eq!(o.status.code(), Some(4));
    assert!(stderr(&o).contains("running unmanaged"), "{}", stderr(&o));
    let strict = crew_with(&socket, &["run", "--", "true"], &[("CREW_STRICT", "1")]);
    assert_eq!(strict.status.code(), Some(125));
    assert!(stderr(&strict).contains("usher is not running"));
}

#[test]
fn unmanaged_runs_still_hide_the_owners_display() {
    let socket = std::env::temp_dir().join("crew-no-such-usher.sock");
    let o = crew_with(
        &socket,
        &["run", "--", "sh", "-c", "echo \"[$WAYLAND_DISPLAY]\""],
        &[("WAYLAND_DISPLAY", "wayland-9")],
    );
    assert_eq!(stdout(&o).trim(), "[]");
}

#[test]
fn unknown_subcommands_exec_crew_plugins_or_fail_clearly() {
    let socket = std::env::temp_dir().join("crew-no-such-usher.sock");
    let o = crew_with(&socket, &["no-such-plugin"], &[]);
    assert_eq!(o.status.code(), Some(125));
    assert!(stderr(&o).contains("crew-no-such-plugin"));
}

#[test]
fn sigterm_to_crew_run_kills_the_scope_and_exits_143() {
    let Some(d) = Daemon::start() else { return };
    let mut run = Command::new(CREW)
        .args(["run", "--", "sleep", "30"])
        .env("CREW_SOCKET", &d.socket)
        .env_remove("CREW_JOB")
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !stdout(&d.crew(&["top"])).contains("RUNNING (1)") {
        assert!(Instant::now() < deadline, "sleep never started");
        std::thread::sleep(Duration::from_millis(50));
    }
    // Let the supervisor install its handler and the scope settle.
    std::thread::sleep(Duration::from_millis(300));
    let pid = run.id().to_string();
    assert!(
        Command::new("kill")
            .args(["-TERM", &pid])
            .status()
            .unwrap()
            .success()
    );
    let started = Instant::now();
    let status = loop {
        if let Some(s) = run.try_wait().unwrap() {
            break s;
        }
        if started.elapsed() > Duration::from_secs(10) {
            let _ = run.kill();
            panic!("crew run did not exit after SIGTERM");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(status.code(), Some(143));
    assert!(stdout(&d.crew(&["top"])).contains("RUNNING (0)"));
}

#[test]
fn a_crew_run_inside_a_granted_job_runs_in_that_job_instead_of_queueing_behind_it() {
    let Some(d) = Daemon::start() else { return };
    assert!(d.crew(&["owner", "away"]).status.success());
    let inner = format!("{CREW} run -- sh -c 'echo inner:$CREW_JOB'");
    let out = crew_with(
        &d.socket,
        &[
            "run", "--kind", "bench", "--", "timeout", "20", "sh", "-c", &inner,
        ],
        &[],
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(
        stdout(&out).trim().starts_with("inner:"),
        "{}",
        stdout(&out)
    );
    assert_ne!(stdout(&out).trim(), "inner:");
}

#[derive(serde::Deserialize)]
struct TopJson {
    machine: MachineJson,
    running: Vec<JobJson>,
    queued: Vec<JobJson>,
    agents: Vec<AgentJson>,
    projects: Vec<ProjectJson>,
}

#[derive(serde::Deserialize)]
struct MachineJson {
    owner_present: bool,
    owner_source: String,
}

#[derive(serde::Deserialize)]
struct JobJson {
    id: String,
    kind: String,
    waiting_on: String,
}

#[derive(serde::Deserialize)]
struct AgentJson {
    name: String,
    kind: String,
    project: String,
    processes: u32,
    source: String,
    named: bool,
    has_tty: bool,
    actionable: bool,
    cgroup: String,
}

#[derive(serde::Deserialize)]
struct ProjectJson {
    name: String,
    agents: u32,
}

#[derive(serde::Deserialize)]
struct WaitJson {
    outcome: String,
    condition: String,
    waited_ms: u64,
}

fn top_json(d: &Daemon) -> TopJson {
    let o = d.crew(&["top", "--json"]);
    assert!(o.status.success(), "{}", stderr(&o));
    serde_json::from_str(stdout(&o).trim()).unwrap_or_else(|e| panic!("{e}: {}", stdout(&o)))
}

fn until<T>(what: &str, mut probe: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(v) = probe() {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn wait_json(d: &Daemon, args: &[&str]) -> (Option<i32>, WaitJson) {
    let o = d.crew(args);
    let parsed = serde_json::from_str(stdout(&o).trim())
        .unwrap_or_else(|e| panic!("{e}: {} / {}", stdout(&o), stderr(&o)));
    (o.status.code(), parsed)
}

fn spawn_crew(d: &Daemon, args: &[&str]) -> Child {
    Command::new(CREW)
        .args(args)
        .env("CREW_SOCKET", &d.socket)
        .env_remove("CREW_JOB")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

#[test]
fn top_json_is_one_typed_record_with_enum_names() {
    let Some(d) = Daemon::start() else { return };
    let t = top_json(&d);
    assert!(t.machine.owner_present && t.machine.owner_source == "default");
    assert!(t.running.is_empty() && t.queued.is_empty());
    assert!(d.crew(&["owner", "away", "--json"]).status.success());
    assert_eq!(
        stdout(&d.crew(&["owner", "away", "--json"])).trim(),
        r#"{"ok":true,"message":"owner away (manual)"}"#
    );
    let t = top_json(&d);
    assert!(!t.machine.owner_present && t.machine.owner_source == "manual");
}

#[test]
fn run_json_reports_granted_and_done_on_stderr_and_leaves_stdout_alone() {
    let Some(d) = Daemon::start() else { return };
    let o = d.crew(&["run", "--json", "--", "sh", "-c", "echo hi; exit 5"]);
    assert_eq!(o.status.code(), Some(5));
    assert_eq!(stdout(&o), "hi\n");
    let events: Vec<String> = stderr(&o).lines().map(str::to_string).collect();
    assert!(
        events[0].starts_with(r#"{"event":"granted","job_id":""#),
        "{events:?}"
    );
    let last = events.last().unwrap();
    assert!(
        last.starts_with(r#"{"event":"done","job_id":""#) && last.contains(r#""exit_code":5"#),
        "{last}"
    );
    let refused = d.crew(&["run", "--json", "--display", "--kind", "game", "--", "true"]);
    assert_eq!(refused.status.code(), Some(75));
    assert!(
        stderr(&refused)
            .contains(r#""event":"error","kind":"refused","refusal":"display_forbidden""#),
        "{}",
        stderr(&refused)
    );
}

#[test]
fn wait_blocks_on_the_usher_and_returns_when_the_condition_holds() {
    let Some(d) = Daemon::start() else { return };
    assert!(d.crew(&["owner", "away"]).status.success());
    let mut first = spawn_crew(&d, &["run", "--kind", "bench", "--", "sleep", "2"]);
    until("the first bench to run", || {
        (top_json(&d).running.len() == 1).then_some(())
    });
    let mut second = spawn_crew(&d, &["run", "--kind", "bench", "--", "true"]);
    let queued = until("the second bench to queue", || top_json(&d).queued.pop());
    assert_eq!(
        (queued.kind.as_str(), queued.waiting_on.as_str()),
        ("bench", "exclusive")
    );

    let (code, w) = wait_json(&d, &["wait", "queue-empty", "--timeout", "300ms", "--json"]);
    assert_eq!(
        (code, w.outcome.as_str(), w.condition.as_str()),
        (Some(124), "timed_out", "queue_empty")
    );
    assert!(w.waited_ms >= 250, "{}", w.waited_ms);
    let (code, w) = wait_json(
        &d,
        &["wait", "job", "no-such-job", "--timeout", "200ms", "--json"],
    );
    assert_eq!((code, w.outcome.as_str()), (Some(124), "timed_out"));

    let job = spawn_crew(
        &d,
        &["wait", "job", &queued.id, "--timeout", "30s", "--json"],
    );
    let empty = spawn_crew(&d, &["wait", "queue-empty", "--timeout", "30s", "--json"]);
    for waiter in [job, empty] {
        let o = waiter.wait_with_output().unwrap();
        assert_eq!(
            o.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&o.stderr)
        );
        let w: WaitJson = serde_json::from_str(String::from_utf8_lossy(&o.stdout).trim()).unwrap();
        assert_eq!(w.outcome, "satisfied");
        assert!(
            w.waited_ms >= 300 && w.waited_ms < 10_000,
            "{}",
            w.waited_ms
        );
    }
    assert!(first.wait().unwrap().success() && second.wait().unwrap().success());
    let (code, w) = wait_json(&d, &["wait", "job", &queued.id, "--json"]);
    assert_eq!((code, w.outcome.as_str()), (Some(0), "satisfied"));
}

#[test]
fn wait_owner_away_follows_the_owner_and_is_notified_at_once() {
    let Some(d) = Daemon::start() else { return };
    assert!(d.crew(&["owner", "here"]).status.success());
    let (code, w) = wait_json(&d, &["wait", "owner-away", "--timeout", "200ms", "--json"]);
    assert_eq!((code, w.outcome.as_str()), (Some(124), "timed_out"));
    let waiter = spawn_crew(&d, &["wait", "owner-away", "--timeout", "30s", "--json"]);
    std::thread::sleep(Duration::from_millis(300));
    let flipped = Instant::now();
    assert!(d.crew(&["owner", "away"]).status.success());
    let o = waiter.wait_with_output().unwrap();
    assert_eq!(o.status.code(), Some(0));
    assert!(
        flipped.elapsed() < Duration::from_millis(1500),
        "the usher must notify, not be polled: {:?}",
        flipped.elapsed()
    );
}

struct FakeAgent {
    pid: u32,
}

impl Drop for FakeAgent {
    fn drop(&mut self) {
        let _ = Command::new("kill")
            .args(["-TERM", "--", &format!("-{}", self.pid)])
            .status();
    }
}

/// A process the kernel names `claude` (a symlink to sh) in its own session, detached from the test's own claude ancestors, with a child.
fn fake_agent(root: &Path, agent: &str) -> Option<FakeAgent> {
    let repo = root.join(agent);
    std::fs::create_dir_all(repo.join("src")).ok()?;
    std::fs::write(repo.join(".git"), "gitdir: nowhere").ok()?;
    let link = root.join(format!("claude-{agent}")).join("claude");
    std::fs::create_dir_all(link.parent()?).ok()?;
    std::os::unix::fs::symlink(std::fs::canonicalize("/bin/sh").ok()?, &link).ok()?;
    let pidfile = root.join(format!("{agent}.pid"));
    let script = format!(
        "echo $$ > {}; exec {} -c 'sleep 120 & wait'",
        pidfile.display(),
        link.display()
    );
    let status = Command::new("setsid")
        .args(["-f", "sh", "-c", &script])
        .current_dir(repo.join("src"))
        .env("CREW_AGENT", agent)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?;
    if !status.success() {
        return None;
    }
    let pid = until("the fake agent's pid", || {
        std::fs::read_to_string(&pidfile).ok()?.trim().parse().ok()
    });
    Some(FakeAgent { pid })
}

#[test]
fn agent_sessions_are_observed_per_project_and_never_touched() {
    let Some(d) = Daemon::start_with(
        "[adopt]\nmove = true\nfreeze = true\nfreeze_psi_cpu = 0.0\nprotect = [\"fake-protected\"]\n",
    ) else {
        return;
    };
    let root = tempfile::tempdir_in("/projects/.hz-tmp")
        .or_else(|_| tempfile::tempdir())
        .unwrap();
    let (Some(busy), Some(guarded)) = (
        fake_agent(root.path(), "fake-busy"),
        fake_agent(root.path(), "fake-protected"),
    ) else {
        eprintln!("skipped: cannot start a fake claude (setsid, /bin/sh)");
        return;
    };
    let cgroup_before = std::fs::read_to_string(format!("/proc/{}/cgroup", busy.pid)).unwrap();
    let seen = until("both fake sessions in top", || {
        let t = top_json(&d);
        let found = |n: &str| t.agents.iter().any(|a| a.name == n && a.processes >= 2);
        (found("fake-busy") && found("fake-protected")).then_some(t)
    });
    let a = seen.agents.iter().find(|a| a.name == "fake-busy").unwrap();
    assert_eq!(
        (a.kind.as_str(), a.project.as_str(), a.named, a.has_tty),
        ("claude", "fake-busy", true, false)
    );
    assert!(a.actionable && a.source == "proc" && !a.cgroup.is_empty());
    let g = seen
        .agents
        .iter()
        .find(|a| a.name == "fake-protected")
        .unwrap();
    assert!(!g.actionable, "a protected agent is never actionable");
    assert!(
        seen.projects
            .iter()
            .any(|p| p.name == "fake-busy" && p.agents == 1)
    );

    let log_path = d.dir.path().join("state/crew/usher.log");
    let log = until("the dry-run lines", || {
        let t = std::fs::read_to_string(&log_path).unwrap_or_default();
        t.contains("would freeze agent fake-busy").then_some(t)
    });
    assert!(log.contains("ADOPT would move agent fake-busy"), "{log}");
    assert!(!log.contains("agent fake-protected"), "{log}");
    assert_eq!(
        std::fs::read_to_string(format!("/proc/{}/cgroup", busy.pid)).unwrap(),
        cgroup_before
    );
    let state = std::fs::read_to_string(format!("/proc/{}/stat", busy.pid)).unwrap();
    assert!(
        !state.contains(") T ") && !state.contains(") Z "),
        "{state}"
    );
    drop(guarded);
}
