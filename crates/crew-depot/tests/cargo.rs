//! `crew-depot cargo` on a tiny temp crate: with no mbx, with a stand-in mbx, with the real mbx, and under a real usher.
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const DEPOT: &str = env!("CARGO_BIN_EXE_crew-depot");

/// Scratch dirs go under `CREW_TEST_TMP` when set, else the system temp dir.
fn tempdir(prefix: &str) -> tempfile::TempDir {
    let base = std::env::var_os("CREW_TEST_TMP").map_or_else(std::env::temp_dir, PathBuf::from);
    fs::create_dir_all(&base).unwrap();
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(base)
        .unwrap()
}

fn script(path: &Path, body: &str) {
    fs::write(path, format!("#!/bin/sh\n{body}")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn has_mbx(dir: &Path) -> bool {
    dir.join("mbx").is_file()
}

struct Fixture {
    dir: tempfile::TempDir,
    project: PathBuf,
    tools: PathBuf,
    path: std::ffi::OsString,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempdir("crew-depot-it-");
        let project = dir.path().join("tiny");
        fs::create_dir_all(project.join("src")).unwrap();
        fs::write(
            project.join("Cargo.toml"),
            "[package]\nname = \"tiny\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        fs::write(
            project.join("src/main.rs"),
            "fn main() { println!(\"hi\"); }\n",
        )
        .unwrap();
        fs::create_dir_all(dir.path().join("cfg/crew")).unwrap();
        // A `cargo` that reaches the toolchain without putting its directory (and any mbx beside it) on the PATH crew-depot sees.
        let tools = dir.path().join("tools");
        fs::create_dir_all(&tools).unwrap();
        let real = std::env::var("CARGO").expect("cargo sets CARGO for tests");
        let real_dir = Path::new(&real).parent().unwrap().display().to_string();
        script(
            &tools.join("cargo"),
            &format!("PATH=\"{real_dir}:$PATH\"\nunset CARGO_HOME\nexec \"{real}\" \"$@\"\n"),
        );
        let kept: Vec<PathBuf> =
            std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
                .filter(|d| !has_mbx(d))
                .collect();
        let path = std::env::join_paths(std::iter::once(tools.clone()).chain(kept)).unwrap();
        Self {
            dir,
            project,
            tools,
            path,
        }
    }

    fn config(&self, depot_toml: &str) {
        fs::write(self.dir.path().join("cfg/crew/depot.toml"), depot_toml).unwrap();
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(DEPOT);
        c.args(args)
            .current_dir(&self.project)
            .env("PATH", &self.path)
            .env("XDG_CONFIG_HOME", self.dir.path().join("cfg"))
            .env("XDG_CACHE_HOME", self.dir.path().join("cache"))
            .env("XDG_STATE_HOME", self.dir.path().join("state"))
            .env("CARGO_HOME", self.dir.path().join("cargo-home"))
            .env("CREW_SOCKET", self.dir.path().join("no-usher.sock"))
            .env_remove("CREW_STRICT")
            .env_remove("CREW_JOB")
            .env_remove("RUSTC_WRAPPER")
            .env_remove("CARGO_BUILD_JOBS")
            .env_remove("CARGO_BUILD_RUSTC_WRAPPER")
            .stdin(Stdio::null());
        // The test may itself run under mbx and rustup; nothing of theirs may reach what is under test.
        for (k, _) in std::env::vars() {
            if k.starts_with("MBX_")
                || k.starts_with("CARGO_PROFILE_")
                || k.starts_with("RUSTUP_TOOLCHAIN")
            {
                c.env_remove(k);
            }
        }
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    fn built(&self) -> bool {
        self.project.join("target/debug/tiny").is_file()
    }

    /// A stand-in mbx: records its arguments and the environment it was given, writes a build report, then runs cargo.
    fn fake_mbx(&self) -> PathBuf {
        let p = self.dir.path().join("fake-mbx");
        let log = self.dir.path().join("mbx.log");
        script(
            &p,
            &format!(
                "echo \"args: $*\" > {log}\nenv | grep -E '^(CARGO_BUILD_JOBS|MBX_|RUSTUP_TOOLCHAIN|RUSTC_WRAPPER|CARGO_PROFILE_)' | sort >> {log}\n\
                 [ -n \"$MBX_STATS_REPORT\" ] && echo '{{\"version\":5,\"session_duration_ns\":2000000000,\"hits\":3,\"misses\":1,\"unconsulted\":2,\"bypasses\":{{\"compiler-query\":1}}}}' > \"$MBX_STATS_REPORT\"\n\
                 unset RUSTUP_TOOLCHAIN\nexec cargo \"$@\"\n",
                log = log.display()
            ),
        );
        p
    }

    fn mbx_log(&self) -> String {
        fs::read_to_string(self.dir.path().join("mbx.log")).unwrap_or_default()
    }
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn without_mbx_it_builds_with_plain_cargo_and_says_so() {
    let f = Fixture::new();
    let out = f.run(&["cargo", "build"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(f.built());
    assert!(
        stderr(&out).contains("mbx not found; building with plain cargo"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn a_cargo_failure_is_the_exit_code() {
    let f = Fixture::new();
    fs::write(f.project.join("src/main.rs"), "fn main() { nope }\n").unwrap();
    let out = f.run(&["cargo", "build"]);
    assert_eq!(out.status.code(), Some(101), "{}", stderr(&out));
}

#[test]
fn mbx_off_never_calls_mbx_even_when_it_is_configured() {
    let f = Fixture::new();
    let mbx = f.fake_mbx();
    f.config(&format!(
        "[depot]\nmbx = \"off\"\nmbx_path = \"{}\"\n",
        mbx.display()
    ));
    let out = f.run(&["cargo", "build"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(f.built() && f.mbx_log().is_empty());
}

#[test]
fn a_required_mbx_that_is_missing_is_an_error_and_builds_nothing() {
    let f = Fixture::new();
    f.config("[depot]\nmbx = \"required\"\n");
    let out = f.run(&["cargo", "build"]);
    assert_eq!(out.status.code(), Some(125));
    assert!(stderr(&out).contains("required") && !f.built());
}

#[test]
fn mbx_gets_the_cargo_arguments_the_pin_and_the_profile_overrides() {
    let f = Fixture::new();
    let mbx = f.fake_mbx();
    f.config(&format!(
        "[depot]\nmbx_path = \"{}\"\ntoolchain = \"stable\"\n[depot.profiles.dev]\nlto = \"off\"\ncodegen-units = 256\n",
        mbx.display()
    ));
    let out = f
        .command(&["cargo", "build", "--profile", "dev"])
        .env("RUSTC_WRAPPER", "/nonexistent/sccache")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(f.built());
    let log = f.mbx_log();
    assert!(log.contains("args: build --profile dev"), "{log}");
    assert!(log.contains("RUSTUP_TOOLCHAIN=stable"), "{log}");
    assert!(log.contains("CARGO_PROFILE_DEV_LTO=off"), "{log}");
    assert!(log.contains("CARGO_PROFILE_DEV_CODEGEN_UNITS=256"), "{log}");
    assert!(
        !log.contains("RUSTC_WRAPPER"),
        "the leftover wrapper must not reach mbx: {log}"
    );
    assert!(
        stderr(&out).contains("ignoring RUSTC_WRAPPER=/nonexistent/sccache"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn the_project_crew_toml_overrides_the_user_settings() {
    let f = Fixture::new();
    let mbx = f.fake_mbx();
    f.config(&format!(
        "[depot]\nmbx_path = \"{}\"\ntoolchain = \"nightly\"\n",
        mbx.display()
    ));
    fs::write(
        f.project.join("crew.toml"),
        "[depot]\ntoolchain = \"stable\"\n",
    )
    .unwrap();
    assert!(f.run(&["cargo", "build"]).status.success());
    assert!(
        f.mbx_log().contains("RUSTUP_TOOLCHAIN=stable"),
        "{}",
        f.mbx_log()
    );
}

#[test]
fn a_build_report_from_mbx_lands_in_this_projects_statistics() {
    let f = Fixture::new();
    f.config(&format!(
        "[depot]\nmbx_path = \"{}\"\n",
        f.fake_mbx().display()
    ));
    assert!(f.run(&["cargo", "build"]).status.success());
    assert!(f.run(&["cargo", "check"]).status.success());
    let out = f.run(&["build", "stats", "--json"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["builds"], 2);
    assert_eq!(v["last_command"], "check");
    assert_eq!(v["last"]["hits"], 3);
    assert_eq!(v["total"]["hits"], 6);
    assert_eq!(v["total"]["bypasses"], 0, "routine probes are not misses");
    let text = String::from_utf8(f.run(&["build", "stats"]).stdout).unwrap();
    assert!(text.contains("2 through crew cargo"), "{text}");
    let leftovers = fs::read_dir(f.dir.path().join("cache/crew/stats"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with("run-"))
        .count();
    assert_eq!(leftovers, 0, "temporary reports are removed");
}

#[test]
fn stats_for_a_project_that_never_built_through_crew_say_so() {
    let f = Fixture::new();
    let out = f.run(&["build", "stats"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("no build of this project"));
}

#[test]
fn why_without_mbx_is_a_clear_error() {
    let f = Fixture::new();
    let out = f.run(&["build", "why", "serde"]);
    assert_eq!(out.status.code(), Some(125));
    assert!(
        stderr(&out).contains("mbx is not installed"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn report_lists_no_dependency_units_for_two_path_only_projects() {
    let f = Fixture::new();
    let other = f.dir.path().join("other");
    fs::create_dir_all(other.join("src")).unwrap();
    fs::copy(f.project.join("Cargo.toml"), other.join("Cargo.toml")).unwrap();
    fs::copy(f.project.join("src/main.rs"), other.join("src/main.rs")).unwrap();
    for dir in [&f.project, &other] {
        let lock = Command::new(f.tools.join("cargo"))
            .arg("generate-lockfile")
            .current_dir(dir)
            .env("PATH", &f.path)
            .output()
            .unwrap();
        assert!(
            lock.status.success(),
            "{}",
            String::from_utf8_lossy(&lock.stderr)
        );
    }
    let out = f.run(&[
        "build",
        "report",
        "--shared",
        other.to_str().unwrap(),
        "--json",
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["units"].as_array().unwrap().len(), 0);
    assert_eq!(v["rustc_here"], v["rustc_there"]);
    let missing = f.run(&["build", "report", "--shared", "/nonexistent/dir"]);
    assert_eq!(missing.status.code(), Some(125));
}

/// The real mbx, with its cache in the scratch dir; skipped when it is not installed.
#[test]
fn the_real_mbx_builds_and_the_statistics_follow() {
    let Some(mbx) = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|d| d.join("mbx"))
        .chain([Path::new(&std::env::var("HOME").unwrap_or_default()).join(".cargo/bin/mbx")])
        .find(|p| p.is_file())
    else {
        eprintln!("skipped: mbx is not installed");
        return;
    };
    let f = Fixture::new();
    f.config(&format!("[depot]\nmbx_path = \"{}\"\n", mbx.display()));
    let build = |args: &[&str]| {
        f.command(args)
            .env("MBX_CACHE_DIR", f.dir.path().join("mbx-cache"))
            .output()
            .unwrap()
    };
    let out = build(&["cargo", "build"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(f.built());
    let stats = build(&["build", "stats", "--json"]);
    let v: serde_json::Value = serde_json::from_slice(&stats.stdout).unwrap();
    assert_eq!(v["builds"], 1, "{}", String::from_utf8_lossy(&stats.stdout));
    let why = build(&["build", "why", "tiny"]);
    assert!(why.status.success(), "{}", stderr(&why));
}

fn skip(why: &str) {
    assert!(
        std::env::var_os("CREW_REQUIRE_SCOPES").is_none(),
        "required but unavailable: {why}"
    );
    eprintln!("skipped: {why}");
}

fn scopes_available() -> bool {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    Command::new("systemd-run")
        .args(["--user", "--scope", "--quiet"])
        .arg(format!(
            "--unit=crew-test-probe-depot-{}-{nanos}",
            std::process::id()
        ))
        .args(["--", "true"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[test]
fn an_usher_build_grant_sets_the_job_count_and_the_priority_by_owner_presence() {
    use crew_usher::config::Config;
    use crew_usher::server::{IdleSource, Paths, Server};
    use crew_usher_client::{Client, OwnerArg};

    if !scopes_available() {
        return skip("systemd-run --user --scope is unavailable");
    }
    let f = Fixture::new();
    f.config(&format!(
        "[depot]\nmbx_path = \"{}\"\n",
        f.fake_mbx().display()
    ));
    let cfg = Config {
        min_free_mem_gib: 0,
        watched_paths: vec![],
        min_free_disk_gib: Default::default(),
        tmp_root: f.dir.path().join("jobtmp").display().to_string(),
        ..Config::default()
    };
    let paths = Paths::in_state(f.dir.path().join("usher.sock"), f.dir.path());
    let server = Server::start(cfg, &paths, IdleSource::Off, false).unwrap();
    let granted = |args: &[&str]| {
        let out = f
            .command(args)
            .env("CREW_SOCKET", &paths.socket)
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", stderr(&out));
        assert!(
            !stderr(&out).contains("running unmanaged"),
            "{}",
            stderr(&out)
        );
        f.mbx_log()
    };
    // The kind's own expectation is four cores; an owner who is present gets the low priority.
    let log = granted(&["cargo", "build"]);
    assert!(log.contains("CARGO_BUILD_JOBS=4"), "{log}");
    assert!(log.contains("MBX_SCHEDULER_PRIORITY=low"), "{log}");

    Client::new(&paths.socket)
        .set_owner(OwnerArg::Away)
        .unwrap();
    let log = granted(&["cargo", "check"]);
    assert!(log.contains("CARGO_BUILD_JOBS=4"), "{log}");
    assert!(!log.contains("MBX_SCHEDULER_PRIORITY"), "{log}");
    server.stop();
}
