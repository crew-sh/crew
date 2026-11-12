use std::path::Path;
use std::process::{Command, Output};

fn plan(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_crew-plan"))
        .arg("--root")
        .arg(dir)
        .args(args)
        .env("CREW_AGENT", "tester")
        .output()
        .unwrap()
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("crew.toml"),
        "[plan]\nrequest = \"ship it\"\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("plan.toml"),
        r#"
[[step]]
id = "one"
title = "first"
kind = "asked"
owns = ["a/**"]
budget = { builds = 1 }

[[step]]
id = "two"
title = "second"
kind = "extra"
deps = ["one"]
owns = ["b/**"]
"#,
    )
    .unwrap();
    dir
}

#[test]
fn the_cli_walks_a_step_from_plan_to_landed() {
    let d = project();
    let p = d.path();
    assert_eq!(plan(p, &["check"]).status.code(), Some(0));
    assert_eq!(out(&plan(p, &["next"])).trim(), "one\tasked\tfirst");
    assert_eq!(
        plan(p, &["status", "two", "running"]).status.code(),
        Some(1)
    );
    assert_eq!(plan(p, &["may-run", "one", "build"]).status.code(), Some(0));
    assert_eq!(
        plan(p, &["use", "one", "--builds", "1"]).status.code(),
        Some(0)
    );
    let refused = plan(p, &["may-run", "one", "build"]);
    assert_eq!(refused.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&refused.stderr).contains("budget"));
    assert_eq!(
        plan(p, &["status", "one", "built", "--detail", "ok"])
            .status
            .code(),
        Some(0)
    );
    assert_eq!(
        plan(p, &["landed", "one", "abc", "--batch", "b1"])
            .status
            .code(),
        Some(0)
    );
    assert_eq!(out(&plan(p, &["next"])), "", "two waits on approval");
    // The agent that runs this test cannot approve; the owner does, from a terminal.
    let refused = plan(p, &["approve", "two", "--note", "go"]);
    assert_eq!(refused.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&refused.stderr).contains("approvals come from the owner"));
    assert_eq!(out(&plan(p, &["next"])), "");
    let owner = on_terminal(p, &["approve", "two", "--note", "go"], "two\n", &[]);
    assert_eq!(owner.code, Some(0), "{}", owner.screen);
    assert_eq!(
        out(&plan(p, &["next", "--n", "1"])).trim(),
        "two\textra\tsecond"
    );

    let show = plan(p, &["show", "--json"]);
    let v: serde_json::Value = serde_json::from_slice(&show.stdout).unwrap();
    assert_eq!(v["steps"][0]["status"], "landed");
    assert_eq!(v["steps"][0]["landed_sha"], "abc");
    assert_eq!(v["summary"]["kinds"][0]["done"], 1);
    assert!(out(&plan(p, &["show"])).starts_with("asked 1/1 · extra 0/1"));
    assert!(out(&plan(p, &["report"])).contains("request: ship it"));
    let md = out(&plan(p, &["render"]));
    assert!(
        md.contains("- [x] `one`") && md.contains("**[EXTRA]**"),
        "{md}"
    );
}

/// What a command that ran on a terminal exited with and showed.
struct Ran {
    code: Option<i32>,
    screen: String,
}

/// Runs `crew-plan` with its stdin and stdout on a pseudo-terminal, typing `keys`, as the owner would; no CREW_AGENT.
fn on_terminal(dir: &Path, args: &[&str], keys: &str, env: &[(&str, &str)]) -> Ran {
    use std::io::{Read, Write};
    use std::os::fd::{FromRawFd, OwnedFd};
    let (mut master, slave) = {
        let (mut m, mut s) = (0, 0);
        // SAFETY: openpty fills both descriptors, which are wrapped at once.
        let rc = unsafe {
            libc::openpty(
                &mut m,
                &mut s,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
            )
        };
        assert_eq!(rc, 0);
        unsafe { (std::fs::File::from_raw_fd(m), OwnedFd::from_raw_fd(s)) }
    };
    master.write_all(keys.as_bytes()).unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_crew-plan"));
    cmd.arg("--root")
        .arg(dir)
        .args(args)
        .env_remove("CREW_AGENT")
        .env_remove("CREW_JOB")
        .env_remove("CREW_OWNER_TOKEN")
        .stdin(slave.try_clone().unwrap())
        .stdout(slave.try_clone().unwrap())
        .stderr(std::process::Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let child = cmd.spawn().unwrap();
    // The command still owns copies of the slave; every one must close or reading the master never ends.
    drop(cmd);
    drop(slave);
    let out = child.wait_with_output().unwrap();
    // Reading after the child is gone returns what was echoed, then fails once the slave closed.
    let mut screen = String::new();
    let mut buf = [0u8; 4096];
    while let Ok(n @ 1..) = master.read(&mut buf) {
        screen.push_str(&String::from_utf8_lossy(&buf[..n]));
        if screen.len() > 1 << 16 {
            break;
        }
    }
    screen.push_str(&String::from_utf8_lossy(&out.stderr));
    Ran {
        code: out.status.code(),
        screen,
    }
}

fn extra_ready(p: &Path) -> bool {
    out(&plan(p, &["next"])).contains("two")
}

fn dep_landed(p: &Path) {
    assert_eq!(plan(p, &["status", "one", "built"]).status.code(), Some(0));
    assert_eq!(plan(p, &["landed", "one", "abc"]).status.code(), Some(0));
}

#[test]
fn the_owner_must_type_the_target_back_and_a_job_or_pipe_cannot_approve() {
    let d = project();
    let p = d.path();
    dep_landed(p);
    let wrong = on_terminal(p, &["approve", "two"], "yes\n", &[]);
    assert_eq!(wrong.code, Some(1), "{}", wrong.screen);
    assert!(!extra_ready(p));
    let in_job = on_terminal(p, &["approve", "two"], "two\n", &[("CREW_JOB", "some-job")]);
    assert_eq!(in_job.code, Some(1), "{}", in_job.screen);
    assert!(in_job.screen.contains("approvals come from the owner"));
    let agent = on_terminal(p, &["approve", "two"], "two\n", &[("CREW_AGENT", "worker")]);
    assert_eq!(agent.code, Some(1), "{}", agent.screen);
    assert!(!extra_ready(p));
    let owner = on_terminal(p, &["approve", "two"], "two\n", &[]);
    assert_eq!(owner.code, Some(0), "{}", owner.screen);
    assert!(extra_ready(p));
}

#[test]
fn a_scripted_owner_needs_the_token_file_and_the_matching_variable() {
    let d = project();
    let p = d.path();
    dep_landed(p);
    let cfg = tempfile::tempdir().unwrap();
    let token = |env: &[(&str, &str)], flag: bool| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_crew-plan"));
        cmd.arg("--root").arg(p).arg("approve").arg("two");
        if flag {
            cmd.arg("--owner-confirmed");
        }
        cmd.env("XDG_CONFIG_HOME", cfg.path())
            .env("CREW_AGENT", "scripted");
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.output().unwrap()
    };
    // The owner makes the token at a terminal; an agent cannot.
    let made = on_terminal(
        p,
        &["owner-token"],
        "",
        &[("XDG_CONFIG_HOME", cfg.path().to_str().unwrap())],
    );
    assert_eq!(made.code, Some(0), "{}", made.screen);
    let file = cfg.path().join("crew/owner-token");
    let secret = std::fs::read_to_string(&file).unwrap().trim().to_string();
    assert_eq!(
        std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(&file).unwrap().permissions())
            & 0o777,
        0o600
    );
    assert_eq!(token(&[], true).status.code(), Some(1), "no variable");
    assert_eq!(
        token(&[("CREW_OWNER_TOKEN", "guess")], true).status.code(),
        Some(1)
    );
    assert_eq!(
        token(&[("CREW_OWNER_TOKEN", &secret)], false).status.code(),
        Some(1),
        "the flag is required"
    );
    assert!(!extra_ready(p));
    assert_eq!(
        token(&[("CREW_OWNER_TOKEN", &secret)], true).status.code(),
        Some(0)
    );
    assert!(extra_ready(p));
    let agent_token = plan(p, &["owner-token"]);
    assert_eq!(
        agent_token.status.code(),
        Some(1),
        "an agent cannot mint the token"
    );
}

#[test]
fn check_exits_nonzero_on_overlap() {
    let d = project();
    std::fs::write(
        d.path().join("plan.toml"),
        "[[step]]\nid=\"a\"\ntitle=\"a\"\nkind=\"asked\"\nowns=[\"x/**\"]\n[[step]]\nid=\"b\"\ntitle=\"b\"\nkind=\"asked\"\nowns=[\"x/y.rs\"]\n",
    )
    .unwrap();
    let o = plan(d.path(), &["check"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(out(&o).contains("`a` and `b`"));
}
