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
    assert_eq!(
        plan(p, &["approve", "two", "--note", "go"]).status.code(),
        Some(0)
    );
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
