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
    _dir: tempfile::TempDir,
}

impl Daemon {
    fn start() -> Option<Self> {
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
            format!("min_free_mem_gib = 0\nwatched_paths = []\nmin_free_disk_gib = {{}}\ntmp_root = \"{}\"\n", dir.path().join("jobtmp").display()),
        )
        .unwrap();
        let child = Command::new(bin)
            .env("CREW_SOCKET", &socket)
            .env("XDG_CONFIG_HOME", dir.path().join("config"))
            .env("XDG_STATE_HOME", dir.path().join("state"))
            .env_remove("WAYLAND_DISPLAY")
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !socket.exists() {
            assert!(Instant::now() < deadline, "daemon never bound its socket");
            std::thread::sleep(Duration::from_millis(20));
        }
        Some(Self {
            child,
            socket,
            _dir: dir,
        })
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
        .env_remove("CREW_STRICT");
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
