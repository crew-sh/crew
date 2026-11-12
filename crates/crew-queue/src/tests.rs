use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crew_ledger::Status;
use tempfile::TempDir;

use crate::check::{CheckOutcome, CheckRunner};
use crate::lock::FileLock;
use crate::runner::Runner;
use crate::{Ctx, status, submit, worktrees};

/// Scratch dirs go under `CREW_TEST_TMP` when set (a big disk), else the system temp dir.
fn tempdir(prefix: &str) -> TempDir {
    let base = std::env::var_os("CREW_TEST_TMP").map_or_else(std::env::temp_dir, PathBuf::from);
    fs::create_dir_all(&base).unwrap();
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(base)
        .unwrap()
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim_end().to_owned()
}

struct Fx {
    _tmp: TempDir,
    root: PathBuf,
    remote: PathBuf,
    ctx: Ctx,
}

impl Fx {
    /// A repo on `main` with files a..d, a plan of `steps` (id, deps) and `[queue]` using `queue_extra` lines.
    fn new(steps: &[(&str, &[&str])], queue_extra: &str) -> Self {
        let tmp = tempdir("crew-queue-");
        let root = tmp.path().join("repo");
        let remote = tmp.path().join("remote.git");
        fs::create_dir(&root).unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        git(
            tmp.path(),
            &["init", "-q", "--bare", remote.to_str().unwrap()],
        );
        git(
            &root,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        for f in ["a.txt", "b.txt", "c.txt", "d.txt"] {
            fs::write(root.join(f), "base\n").unwrap();
        }
        fs::write(root.join(".gitignore"), ".crew/\n").unwrap();
        let mut plan = String::new();
        for (id, deps) in steps {
            let deps: Vec<String> = deps.iter().map(|d| format!("\"{d}\"")).collect();
            plan.push_str(&format!(
                "[[step]]\nid = \"{id}\"\ntitle = \"{id}\"\nkind = \"asked\"\ndeps = [{}]\n\n",
                deps.join(", ")
            ));
        }
        fs::write(root.join("plan.toml"), plan).unwrap();
        let toml = format!(
            "[plan]\nrequest = \"test\"\n\n[queue]\nintegration_branch = \"integration\"\nbase = \"main\"\n{queue_extra}\n"
        );
        fs::write(root.join("crew.toml"), toml).unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "init"]);
        let ctx = Ctx::at(&root).unwrap();
        Self {
            _tmp: tmp,
            root,
            remote,
            ctx,
        }
    }

    /// Create the step's branch with one commit writing `files`, and submit it.
    fn built(&self, step: &str, files: &[(&str, &str)]) {
        let branch = self.commit_branch(step, files);
        submit::submit(&self.ctx, &branch, step).unwrap();
    }

    fn commit_branch(&self, step: &str, files: &[(&str, &str)]) -> String {
        let wt = self._tmp.path().join(format!("wt-{step}"));
        let branch = self.ctx.step_branch(step);
        crew_git::worktree_add(&self.root, &wt, &branch, "main").unwrap();
        for (name, body) in files {
            fs::write(wt.join(name), body).unwrap();
        }
        git(&wt, &["add", "."]);
        git(&wt, &["commit", "-qm", step]);
        crew_git::worktree_remove(&self.root, &wt, true).unwrap();
        branch
    }

    fn run(&self) -> crate::runner::Summary {
        Runner::new(&self.ctx, CheckRunner::default())
            .run(true)
            .unwrap()
    }

    fn status(&self, step: &str) -> Status {
        self.ctx.ledger().unwrap().state(step).unwrap().status
    }

    fn detail(&self, step: &str) -> String {
        self.ctx.ledger().unwrap().state(step).unwrap().detail
    }

    fn integration_has(&self, file: &str) -> bool {
        crew_git::branch_tip(&self.root, "integration")
            .unwrap()
            .is_some()
            && Command::new("git")
                .arg("-C")
                .arg(&self.root)
                .args(["cat-file", "-e", &format!("integration:{file}")])
                .status()
                .unwrap()
                .success()
    }
}

#[test]
fn a_batch_of_disjoint_branches_lands_in_one_check() {
    let scratch = tempdir("crew-queue-count-");
    let log = scratch.path().join("batch").display().to_string();
    let fx = Fx::new(
        &[("s1", &[]), ("s2", &[]), ("s3", &[])],
        &format!("check = [\"echo x >> {log}\"]"),
    );
    fx.built("s1", &[("a.txt", "one\n")]);
    fx.built("s2", &[("b.txt", "two\n")]);
    fx.built("s3", &[("c.txt", "three\n")]);
    let s = fx.run();
    assert_eq!((s.landed.len(), s.failed.len(), s.batches), (3, 0, 1));
    assert_eq!(fs::read_to_string(&log).unwrap().lines().count(), 1);
    for step in ["s1", "s2", "s3"] {
        let state = fx.ctx.ledger().unwrap().state(step).unwrap();
        assert_eq!(state.status, Status::Landed);
        assert!(
            state
                .landed
                .is_some_and(|l| l.sha.len() == 40 && l.batch_id.starts_with('b'))
        );
    }
    let tip = crew_git::branch_tip(&fx.root, "integration")
        .unwrap()
        .unwrap();
    for branch in ["crew/s1", "crew/s2", "crew/s3"] {
        assert!(crew_git::is_ancestor(&fx.root, branch, &tip).unwrap());
    }
}

#[test]
fn bisect_finds_the_culprit_and_lands_the_rest() {
    let scratch = tempdir("crew-queue-count-");
    let log = scratch.path().join("bisect").display().to_string();
    let check =
        format!("echo x >> {log}; test ! -e BROKEN || {{ echo 'BROKEN is present'; exit 1; }}");
    let fx = Fx::new(
        &[("s1", &[]), ("s2", &[]), ("s3", &[]), ("s4", &[])],
        &format!("check = [\"{check}\"]"),
    );
    fx.built("s1", &[("a.txt", "1\n")]);
    fx.built("s2", &[("b.txt", "2\n")]);
    fx.built("s3", &[("BROKEN", "x\n")]);
    fx.built("s4", &[("d.txt", "4\n")]);
    let s = fx.run();
    assert_eq!(s.landed.len(), 3);
    assert_eq!(s.failed.len(), 1);
    assert_eq!(fx.status("s3"), Status::Failed);
    assert!(
        fx.detail("s3").contains("BROKEN is present"),
        "{}",
        fx.detail("s3")
    );
    for step in ["s1", "s2", "s4"] {
        assert_eq!(fx.status(step), Status::Landed, "{step}");
    }
    assert!(!fx.integration_has("BROKEN"));
    assert!(fx.integration_has("d.txt") && fx.integration_has("a.txt"));
    // 4 -> (2) green, (2) red -> 1 green, 1 red: five checks, not one per branch plus retries
    assert_eq!(fs::read_to_string(&log).unwrap().lines().count(), 5);
}

#[test]
fn branches_that_touch_the_same_file_go_in_separate_batches_and_a_real_conflict_fails_only_the_loser()
 {
    let fx = Fx::new(&[("s1", &[]), ("s2", &[]), ("s3", &[])], "");
    fx.built("s1", &[("a.txt", "from s1\n")]);
    fx.built("s2", &[("a.txt", "from s2\n")]);
    fx.built("s3", &[("b.txt", "fine\n")]);
    let s = fx.run();
    assert_eq!(s.batches, 2);
    assert_eq!(fx.status("s1"), Status::Landed);
    assert_eq!(fx.status("s3"), Status::Landed);
    assert_eq!(fx.status("s2"), Status::Failed);
    assert!(fx.detail("s2").contains("a.txt"), "{}", fx.detail("s2"));
}

#[test]
fn an_already_merged_branch_counts_as_landed() {
    let fx = Fx::new(&[("s1", &[])], "");
    let branch = fx.commit_branch("s1", &[("a.txt", "x\n")]);
    // someone merged it into the integration branch by hand
    let tmp = fx._tmp.path().join("manual");
    crew_git::worktree_add(&fx.root, &tmp, "integration", "main").unwrap();
    git(&tmp, &["merge", "--no-ff", "-qm", "manual", &branch]);
    crew_git::worktree_remove(&fx.root, &tmp, true).unwrap();
    submit::submit(&fx.ctx, &branch, "s1").unwrap();
    let s = fx.run();
    assert_eq!((s.landed.len(), s.failed.len(), s.batches), (1, 0, 0));
    assert_eq!(fx.status("s1"), Status::Landed);
}

#[test]
fn a_step_waits_for_its_deps_and_lands_after_them() {
    let fx = Fx::new(&[("base", &[]), ("child", &["base"])], "");
    fx.built("child", &[("b.txt", "child\n")]);
    let s = fx.run();
    assert_eq!(s.landed.len(), 0);
    assert_eq!(fx.status("child"), Status::Built);
    let report = status::report(&fx.ctx).unwrap();
    assert_eq!(report.steps[0].phase, status::Phase::WaitingOnDeps);
    fx.built("base", &[("a.txt", "base changed\n")]);
    let s = fx.run();
    assert_eq!(
        s.landed
            .iter()
            .map(|(id, _)| id.as_str())
            .collect::<Vec<_>>(),
        ["base", "child"]
    );
}

#[test]
fn a_second_runner_is_refused_while_the_first_holds_the_lock() {
    let fx = Fx::new(&[("s1", &[])], "");
    let _held = FileLock::try_exclusive(&fx.ctx.crew_dir().join("queue.lock"))
        .unwrap()
        .unwrap();
    let err = Runner::new(&fx.ctx, CheckRunner::default())
        .run(true)
        .unwrap_err();
    assert!(err.to_string().contains("another queue runner"), "{err}");
    assert!(status::report(&fx.ctx).unwrap().runner_active);
}

#[test]
fn a_missing_branch_fails_its_step() {
    let fx = Fx::new(&[("s1", &[])], "");
    let branch = fx.commit_branch("s1", &[("a.txt", "x\n")]);
    submit::submit(&fx.ctx, &branch, "s1").unwrap();
    git(&fx.root, &["branch", "-D", &branch]);
    let s = fx.run();
    assert_eq!(s.failed.len(), 1);
    assert!(fx.detail("s1").contains("does not exist"));
}

#[test]
fn green_batches_are_pushed_when_configured() {
    let fx = Fx::new(
        &[("s1", &[])],
        "push = { remote = \"origin\", branch = \"forge/ari\" }",
    );
    fx.built("s1", &[("a.txt", "x\n")]);
    let s = fx.run();
    assert!(s.push_errors.is_empty());
    let remote_tip = git(&fx.remote, &["rev-parse", "forge/ari"]);
    assert_eq!(
        remote_tip,
        crew_git::branch_tip(&fx.root, "integration")
            .unwrap()
            .unwrap()
    );
}

fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let p = dir.join(name);
    fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    Command::new("chmod").arg("+x").arg(&p).status().unwrap();
    p
}

#[test]
fn checks_go_through_crew_run_with_the_build_kind() {
    let fx = Fx::new(&[("s1", &[])], "check = [\"true\"]");
    let log = fx._tmp.path().join("crew.log");
    let fake = script(
        fx._tmp.path(),
        "fake-crew",
        &format!("echo \"$@\" > {}\nshift 4\nexec \"$@\"", log.display()),
    );
    fx.built("s1", &[("a.txt", "x\n")]);
    let runner = Runner::new(
        &fx.ctx,
        CheckRunner {
            crew_bin: Some(fake),
        },
    );
    assert_eq!(runner.run(true).unwrap().landed.len(), 1);
    assert!(
        fs::read_to_string(&log)
            .unwrap()
            .starts_with("run --kind build -- sh -c")
    );
}

#[test]
fn a_refused_check_stops_the_run_and_blames_nobody() {
    let fx = Fx::new(&[("s1", &[])], "check = [\"true\"]");
    let refuse = script(
        fx._tmp.path(),
        "refusing-crew",
        "echo 'usher says no' >&2\nexit 75",
    );
    fx.built("s1", &[("a.txt", "x\n")]);
    let err = Runner::new(
        &fx.ctx,
        CheckRunner {
            crew_bin: Some(refuse),
        },
    )
    .run(true)
    .unwrap_err();
    assert!(err.to_string().contains("did not run"), "{err}");
    assert_eq!(fx.status("s1"), Status::Built);
    // nothing was lost: the next run, with the check able to run, lands it
    assert_eq!(fx.run().landed.len(), 1);
}

#[test]
fn the_check_runner_reports_red_with_the_output_tail_and_stops_at_the_first_failure() {
    let t = tempdir("crew-queue-chk-");
    let cmds: Vec<String> = vec![
        "echo first".into(),
        "seq 1 100; echo oops >&2; exit 3".into(),
        "touch never".into(),
    ];
    let out = CheckRunner::default().run_all(t.path(), &cmds).unwrap();
    let CheckOutcome::Red { command, tail } = out else {
        panic!("{out:?}")
    };
    assert!(command.starts_with("seq"));
    assert_eq!(tail.lines().count(), 60);
    assert!(tail.ends_with("oops"));
    assert!(!t.path().join("never").exists());
}

fn dead_pid() -> u64 {
    let mut child = Command::new("true").spawn().unwrap();
    let pid = u64::from(child.id());
    child.wait().unwrap();
    pid
}

#[test]
fn a_live_owner_holds_its_worktree_but_a_dead_owner_never_does() {
    let fx = Fx::new(&[("s1", &[]), ("s2", &[])], "");
    let me = u64::from(std::process::id());
    let path = worktrees::new(&fx.ctx, "s1", Some(me)).unwrap();
    assert!(path.join("a.txt").exists());
    let err = worktrees::new(&fx.ctx, "s1", Some(1)).unwrap_err();
    assert!(err.to_string().contains("taken"), "{err}");

    worktrees::new(&fx.ctx, "s2", Some(dead_pid())).unwrap();
    let s2 = fx.ctx.worktree_root().join("s2");
    fs::write(s2.join("inflight.txt"), "keep me").unwrap();
    // resuming takes the worktree over with the in-flight edit intact
    assert_eq!(worktrees::new(&fx.ctx, "s2", Some(me)).unwrap(), s2);
    assert_eq!(
        fs::read_to_string(s2.join("inflight.txt")).unwrap(),
        "keep me"
    );
    let rows = status::report(&fx.ctx).unwrap().worktrees;
    assert!(rows.iter().all(|w| !w.dead));
}

#[test]
fn gc_salvages_and_removes_a_dead_worktree_and_leaves_live_ones() {
    let fx = Fx::new(&[("s1", &[]), ("s2", &[])], "");
    let me = u64::from(std::process::id());
    let live = worktrees::new(&fx.ctx, "s1", Some(me)).unwrap();
    let dead = worktrees::new(&fx.ctx, "s2", Some(dead_pid())).unwrap();
    fs::write(dead.join("a.txt"), "edited\n").unwrap();
    fs::write(dead.join("scratch.txt"), "untracked\n").unwrap();
    fs::write(dead.join("b.txt"), "committed\n").unwrap();
    git(&dead, &["add", "b.txt"]);
    git(&dead, &["commit", "-qm", "unlanded work"]);

    let rows = worktrees::gc(&fx.ctx).unwrap();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].removed);
    let saved = rows[0].salvage_dir.clone().unwrap();
    assert!(
        fs::read_to_string(saved.join("tracked.diff"))
            .unwrap()
            .contains("+edited")
    );
    assert!(saved.join("untracked.tar.gz").exists());
    let patches: String = fs::read_dir(saved.join("patches"))
        .unwrap()
        .map(|e| fs::read_to_string(e.unwrap().path()).unwrap())
        .collect();
    assert!(patches.contains("unlanded work"));
    assert!(!dead.exists());
    assert!(live.exists());
    // the branch keeps the commit; the registry forgot the dead one
    assert!(crew_git::branch_tip(&fx.root, "crew/s2").unwrap().is_some());
    assert_eq!(status::report(&fx.ctx).unwrap().worktrees.len(), 1);
}

#[test]
fn done_drops_the_target_dir_and_gc_then_collects_it() {
    let fx = Fx::new(&[("s1", &[])], "");
    let wt = worktrees::new(&fx.ctx, "s1", Some(u64::from(std::process::id()))).unwrap();
    fs::create_dir_all(wt.join("target")).unwrap();
    fs::write(wt.join("target/big"), "x").unwrap();
    worktrees::done(&fx.ctx, "s1").unwrap();
    assert!(!wt.join("target").exists());
    let rows = worktrees::gc(&fx.ctx).unwrap();
    assert!(rows[0].removed && !wt.exists());
}

#[test]
fn salvage_all_dead_writes_per_step_folders_without_removing_anything() {
    let fx = Fx::new(&[("s1", &[])], "");
    let wt = worktrees::new(&fx.ctx, "s1", Some(dead_pid())).unwrap();
    fs::write(wt.join("scratch.txt"), "u\n").unwrap();
    let to = fx._tmp.path().join("out");
    let rows = worktrees::salvage(&fx.ctx, None, true, &to).unwrap();
    assert_eq!(rows.len(), 1);
    assert!(to.join("s1/untracked.tar.gz").exists());
    assert!(wt.exists());
    assert!(worktrees::salvage(&fx.ctx, None, false, &to).is_err());
}

#[test]
fn a_worktree_dir_deleted_behind_git_is_recreated_not_reported_taken() {
    let fx = Fx::new(&[("s1", &[])], "");
    let wt = worktrees::new(&fx.ctx, "s1", Some(dead_pid())).unwrap();
    fs::remove_dir_all(&wt).unwrap();
    let again = worktrees::new(&fx.ctx, "s1", Some(u64::from(std::process::id()))).unwrap();
    assert!(again.join("a.txt").exists());
}

#[test]
fn discover_maps_a_linked_worktree_onto_the_main_queue() {
    let fx = Fx::new(&[("s1", &[])], "");
    let wt = worktrees::new(&fx.ctx, "s1", Some(u64::from(std::process::id()))).unwrap();
    let found = Ctx::discover(&wt).unwrap();
    assert_eq!(found.root, fx.root.canonicalize().unwrap());
}

#[test]
fn submit_refuses_a_missing_branch_and_a_landed_step() {
    let fx = Fx::new(&[("s1", &[])], "");
    assert!(submit::submit(&fx.ctx, "crew/nope", "s1").is_err());
    fx.built("s1", &[("a.txt", "x\n")]);
    fx.run();
    let branch = fx.ctx.step_branch("s1");
    assert!(
        submit::submit(&fx.ctx, &branch, "s1")
            .unwrap_err()
            .to_string()
            .contains("already landed")
    );
}

#[test]
fn queue_file_round_trips_and_survives_concurrent_style_updates() {
    let t = tempdir("crew-queue-store-");
    let store = crate::store::Store::new(t.path());
    store
        .update(|f| {
            f.submissions.push(crate::store::Submission {
                step_id: "a".into(),
                branch: "b".into(),
                submitted_ms: 1,
            })
        })
        .unwrap();
    store
        .update(|f| {
            f.submissions.push(crate::store::Submission {
                step_id: "c".into(),
                branch: "d".into(),
                submitted_ms: 2,
            })
        })
        .unwrap();
    let ids: Vec<String> = store
        .load()
        .unwrap()
        .submissions
        .into_iter()
        .map(|s| s.step_id)
        .collect();
    assert_eq!(ids, ["a", "c"]);
}
