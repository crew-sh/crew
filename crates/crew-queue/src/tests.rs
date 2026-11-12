use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crew_ledger::Status;
use tempfile::TempDir;

use crate::check::{CheckOutcome, CheckRunner};
use crate::lock::FileLock;
use crate::runner::{Runner, Summary, backoff, drive};
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
        git(&root, &["config", "user.name", "t"]);
        git(&root, &["config", "user.email", "t@t"]);
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
        .with_lock_wait(Duration::from_millis(100))
        .run(true)
        .unwrap_err();
    assert!(err.to_string().contains("another queue runner"), "{err}");
    assert!(status::report(&fx.ctx).unwrap().runner_active);
}

#[test]
fn a_runner_that_starts_while_status_peeks_at_the_lock_waits_instead_of_refusing() {
    let fx = Fx::new(&[("s1", &[])], "");
    let path = fx.ctx.crew_dir().join("queue.lock");
    let peek = std::thread::spawn({
        let path = path.clone();
        move || {
            // What `crew queue status` does: take the lock to see who holds it, then let go.
            let held = FileLock::try_exclusive(&path).unwrap().unwrap();
            std::thread::sleep(Duration::from_millis(400));
            drop(held);
        }
    });
    std::thread::sleep(Duration::from_millis(100));
    let summary = Runner::new(&fx.ctx, CheckRunner::default()).run(true);
    peek.join().unwrap();
    assert!(summary.is_ok(), "{summary:?}");
}

fn landed_one() -> Summary {
    Summary {
        landed: vec![("s1".into(), "abc".into())],
        batches: 1,
        ..Summary::default()
    }
}

#[test]
fn a_looping_runner_survives_failed_passes_and_a_single_run_reports_them() {
    let mut script = vec![
        Err(anyhow::anyhow!("usher says no")),
        Err(anyhow::anyhow!("git hiccup")),
        Ok(landed_one()),
        Ok(Summary::default()),
    ]
    .into_iter();
    let mut waits = Vec::new();
    let total = drive(
        false,
        || script.next().unwrap(),
        |d| {
            waits.push(d);
            // Stop at the first idle wait, after the failures were ridden out.
            waits.len() < 3
        },
    )
    .unwrap();
    assert_eq!(total.landed.len(), 1);
    assert!(
        waits[1] > waits[0],
        "the wait grows with each failure: {waits:?}"
    );

    let mut once = vec![Err(anyhow::anyhow!("usher says no"))].into_iter();
    let err = drive(true, || once.next().unwrap(), |_| true).unwrap_err();
    assert!(err.to_string().contains("usher says no"));
}

#[test]
fn the_wait_after_errors_grows_and_is_capped() {
    let waits: Vec<_> = (1..=10).map(backoff).collect();
    assert!(waits.windows(2).all(|w| w[0] <= w[1]), "{waits:?}");
    assert_eq!(waits[9], Duration::from_secs(60));
    assert!(waits[0] < Duration::from_secs(30));
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
    worktrees::done(&fx.ctx, "s1", Some(u64::from(std::process::id()))).unwrap();
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

/// The first ```toml fence after `<!-- test:NAME -->` in the README.
fn readme_block(name: &str) -> String {
    let readme = include_str!("../../../README.md");
    let after = readme
        .split(&format!("<!-- test:{name} -->"))
        .nth(1)
        .unwrap();
    let body = after.split("```toml\n").nth(1).unwrap();
    body.split("```").next().unwrap().to_string()
}

#[test]
fn the_readme_quickstart_files_are_valid() {
    let t = tempdir("crew-readme-");
    let crew_toml = format!(
        "{}\n{}",
        readme_block("crew-toml"),
        readme_block("usher-table")
    );
    fs::write(t.path().join("crew.toml"), &crew_toml).unwrap();
    fs::write(t.path().join("plan.toml"), readme_block("plan-toml")).unwrap();
    let q = crate::config::load(t.path()).unwrap();
    assert_eq!(
        (q.integration_branch.as_str(), q.base.as_str()),
        ("crew/integration", "main")
    );
    let ledger = crew_ledger::Ledger::open(t.path()).unwrap();
    assert_eq!(ledger.plan().steps.len(), 2);
    assert!(crew_ledger::check(&ledger).unwrap().is_empty());
    let rules = crew_usher_client::kind::ProjectRules::parse(&crew_toml, "crew.toml").unwrap();
    let argv = |c: &str| c.split(' ').map(str::to_string).collect::<Vec<_>>();
    assert_eq!(
        crew_usher_client::kind::infer_with(&argv("my-ctl launch --world W"), &rules),
        crew_proto::JobKind::Game
    );
}

#[test]
fn a_submission_for_a_step_the_plan_lost_is_dropped_and_never_merged() {
    let fx = Fx::new(&[("s1", &[]), ("s2", &[])], "");
    fx.built("s1", &[("n1.txt", "one\n")]);
    fx.built("s2", &[("n2.txt", "two\n")]);
    // The step disappears from the plan after it was submitted.
    let plan = fs::read_to_string(fx.root.join("plan.toml")).unwrap();
    let kept: String = plan
        .split("[[step]]")
        .filter(|b| !b.contains("\"s1\""))
        .collect::<Vec<_>>()
        .join("[[step]]");
    fs::write(fx.root.join("plan.toml"), kept).unwrap();
    let tip_before = crew_git::branch_tip(&fx.root, "integration").unwrap();
    let s = fx.run();
    assert_eq!(
        s.landed
            .iter()
            .map(|(id, _)| id.as_str())
            .collect::<Vec<_>>(),
        ["s2"]
    );
    assert!(
        !fx.integration_has("n1.txt"),
        "the orphaned branch was merged"
    );
    assert!(fx.integration_has("n2.txt"));
    assert_ne!(
        tip_before,
        crew_git::branch_tip(&fx.root, "integration").unwrap()
    );
    // And the queue is not stuck: the next pass works.
    assert_eq!(fx.run().batches, 0);
}

#[test]
fn a_branch_with_nothing_beyond_the_base_fails_instead_of_landing() {
    let fx = Fx::new(&[("s1", &[]), ("child", &["s1"])], "");
    git(&fx.root, &["branch", "crew/s1", "main"]);
    submit::submit(&fx.ctx, "crew/s1", "s1").unwrap();
    fx.built("child", &[("b.txt", "child\n")]);
    let s = fx.run();
    assert_eq!(fx.status("s1"), Status::Failed, "{:?}", s);
    assert!(
        fx.detail("s1").contains("no commits beyond"),
        "{}",
        fx.detail("s1")
    );
    assert_ne!(
        fx.status("child"),
        Status::Landed,
        "its dependency never landed"
    );
}

#[test]
fn a_stacked_branch_in_a_red_batch_is_not_recorded_landed_before_the_check() {
    let fx = Fx::new(
        &[("a", &[]), ("b", &["a"])],
        "check = [\"test ! -e BROKEN\"]",
    );
    // b is stacked on a, and a's commit is the broken one.
    let wt = fx._tmp.path().join("wt-stack");
    crew_git::worktree_add(&fx.root, &wt, "crew/a", "main").unwrap();
    fs::write(wt.join("BROKEN"), "x").unwrap();
    git(&wt, &["add", "."]);
    git(&wt, &["commit", "-qm", "a"]);
    git(&wt, &["checkout", "-q", "-b", "crew/b"]);
    fs::write(wt.join("n3.txt"), "b\n").unwrap();
    git(&wt, &["add", "."]);
    git(&wt, &["commit", "-qm", "b"]);
    crew_git::worktree_remove(&fx.root, &wt, true).unwrap();
    submit::submit(&fx.ctx, "crew/a", "a").unwrap();
    submit::submit(&fx.ctx, "crew/b", "b").unwrap();
    let s = fx.run();
    assert_eq!(s.landed.len(), 0, "{s:?}");
    assert!(!fx.integration_has("BROKEN") && !fx.integration_has("n3.txt"));
    assert_ne!(fx.status("b"), Status::Landed);
    assert_eq!(fx.status("a"), Status::Failed);
}

fn killed_check_runner(dir: &Path, tries: &Path) -> CheckRunner {
    // First run is killed by SIGKILL, as the OOM killer would; later runs pass.
    let fake = script(
        dir,
        "kill-once",
        &format!(
            "echo x >> {t}\nif [ \"$(wc -l < {t})\" -le 1 ]; then kill -9 $$; fi\nshift 4\nexec \"$@\"",
            t = tries.display()
        ),
    );
    CheckRunner {
        crew_bin: Some(fake),
    }
}

#[test]
fn a_check_killed_by_a_signal_is_retried_and_blames_no_step() {
    let fx = Fx::new(&[("s1", &[])], "check = [\"true\"]");
    let tries = fx._tmp.path().join("tries");
    fx.built("s1", &[("a.txt", "x\n")]);
    let runner = Runner::new(&fx.ctx, killed_check_runner(fx._tmp.path(), &tries));
    let s = runner.run(true).unwrap();
    assert_eq!((s.landed.len(), s.failed.len()), (1, 0), "{s:?}");
    assert_eq!(fs::read_to_string(&tries).unwrap().lines().count(), 2);
}

#[test]
fn a_check_that_is_killed_every_time_stops_the_runner_without_failing_the_step() {
    let fx = Fx::new(&[("s1", &[])], "check = [\"true\"]");
    let always = script(fx._tmp.path(), "always-killed", "kill -9 $$");
    fx.built("s1", &[("a.txt", "x\n")]);
    let runner = Runner::new(
        &fx.ctx,
        CheckRunner {
            crew_bin: Some(always),
        },
    );
    let err = runner.run(true).unwrap_err();
    assert!(err.to_string().contains("did not run to a result"), "{err}");
    assert_eq!(fx.status("s1"), Status::Built);
    assert!(!fx.integration_has("n4.txt"));
}

#[test]
fn the_default_owner_is_the_agent_session_not_the_callers_throwaway_parent() {
    let chain = [
        (900, "sh".to_string()),
        (800, "bash".to_string()),
        (700, "claude".to_string()),
        (600, "tmux".to_string()),
    ];
    assert_eq!(worktrees::default_owner(&chain, 321), 700);
    assert_eq!(
        worktrees::default_owner(&chain[..2], 321),
        321,
        "no agent above: the session leader"
    );
}

#[test]
fn worktree_new_refuses_ids_that_are_paths_or_not_in_the_plan() {
    let fx = Fx::new(&[("s1", &[])], "");
    let me = Some(u64::from(std::process::id()));
    for bad in [
        "..",
        "../escape",
        "/abs",
        ".integration",
        "-x",
        "not-in-plan",
    ] {
        assert!(worktrees::new(&fx.ctx, bad, me).is_err(), "{bad}");
    }
    assert!(
        !fx.ctx.integration_worktree().exists()
            || fx.ctx.integration_worktree().join("a.txt").exists()
    );
    worktrees::new(&fx.ctx, "s1", me).unwrap();
}

#[test]
fn two_agents_starting_the_same_step_at_once_cannot_both_own_it() {
    let fx = Fx::new(&[("s1", &[])], "");
    let mut sleepers: Vec<_> = (0..2)
        .map(|_| Command::new("sleep").arg("30").spawn().unwrap())
        .collect();
    let owners: Vec<u64> = sleepers.iter().map(|c| u64::from(c.id())).collect();
    let results: Vec<_> = std::thread::scope(|s| {
        let handles: Vec<_> = owners
            .iter()
            .map(|o| {
                let ctx = &fx.ctx;
                s.spawn(move || worktrees::new(ctx, "s1", Some(*o)))
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    for c in &mut sleepers {
        let _ = c.kill();
        let _ = c.wait();
    }
    assert_eq!(
        results.iter().filter(|r| r.is_ok()).count(),
        1,
        "{results:?}"
    );
}

#[test]
fn gc_keeps_a_dead_owners_worktree_while_a_process_works_in_it() {
    let fx = Fx::new(&[("s1", &[])], "");
    let wt = worktrees::new(&fx.ctx, "s1", Some(dead_pid())).unwrap();
    let mut editor = Command::new("sleep")
        .arg("30")
        .current_dir(&wt)
        .spawn()
        .unwrap();
    let rows = worktrees::gc(&fx.ctx).unwrap();
    let _ = editor.kill();
    let _ = editor.wait();
    assert!(!rows[0].removed && wt.exists());
    assert!(worktrees::gc(&fx.ctx).unwrap()[0].removed);
}

/// Peak resident memory of this process, in bytes.
fn peak_rss() -> u64 {
    let status = fs::read_to_string("/proc/self/status").unwrap();
    let kib: u64 = status
        .lines()
        .find_map(|l| l.strip_prefix("VmHWM:"))
        .and_then(|v| v.split_whitespace().next()?.parse().ok())
        .unwrap();
    kib * 1024
}

#[test]
fn a_chatty_check_costs_no_memory_and_its_last_lines_are_still_reported() {
    let t = tempdir("crew-queue-chatty-");
    let before = peak_rss();
    let cmds = vec![
        "yes 'a line of output that nobody reads' | head -c 300000000; echo the-last-line; exit 3"
            .to_string(),
    ];
    let out = CheckRunner::default().run_all(t.path(), &cmds).unwrap();
    let CheckOutcome::Red { tail, .. } = out else {
        panic!("{out:?}")
    };
    assert!(tail.ends_with("the-last-line"), "{tail:?}");
    assert_eq!(tail.lines().count(), 60);
    assert!(
        tail.lines().all(|l| !l.is_empty()),
        "the window must not start on half a line"
    );
    let grown = peak_rss().saturating_sub(before);
    assert!(
        grown < 100 << 20,
        "300 MB of check output grew the process by {} MiB",
        grown >> 20
    );
}

#[test]
fn gc_does_not_remove_a_worktree_taken_over_just_before_the_removal() {
    let fx = Fx::new(&[("s1", &[])], "");
    let wt = worktrees::new(&fx.ctx, "s1", Some(dead_pid())).unwrap();
    let me = u64::from(std::process::id());
    let rows = worktrees::gc_with(&fx.ctx, &|reg| {
        // Another agent resumes the step after gc judged it dead and salvaged it.
        worktrees::new(&fx.ctx, &reg.step_id, Some(me)).unwrap();
        fs::write(wt.join("fresh-edit.txt"), "the new owner's work").unwrap();
    })
    .unwrap();
    assert!(!rows[0].removed, "{rows:?}");
    assert!(wt.join("fresh-edit.txt").exists());
    assert_eq!(status::report(&fx.ctx).unwrap().worktrees.len(), 1);
}

#[test]
fn done_is_refused_for_another_agents_live_worktree_and_allowed_for_its_owner() {
    let fx = Fx::new(&[("s1", &[]), ("s2", &[])], "");
    let me = u64::from(std::process::id());
    let mut other = Command::new("sleep").arg("30").spawn().unwrap();
    let theirs = u64::from(other.id());
    let wt = worktrees::new(&fx.ctx, "s1", Some(theirs)).unwrap();
    fs::create_dir_all(wt.join("target")).unwrap();

    let err = worktrees::done(&fx.ctx, "s1", Some(me)).unwrap_err();
    assert!(err.to_string().contains("owned by"), "{err}");
    assert!(wt.join("target").exists(), "a refused done removes nothing");
    assert!(!status::report(&fx.ctx).unwrap().worktrees[0].dead);

    worktrees::done(&fx.ctx, "s1", Some(theirs)).unwrap();
    assert!(!wt.join("target").exists());

    // A worktree whose owner is gone can be closed by anyone.
    worktrees::new(&fx.ctx, "s2", Some(dead_pid())).unwrap();
    worktrees::done(&fx.ctx, "s2", Some(me)).unwrap();
    let _ = other.kill();
    let _ = other.wait();
}

#[test]
fn a_pass_reads_the_ledger_once_not_once_per_submission_and_dependency() {
    let steps: Vec<(String, Vec<&str>)> = (0..8)
        .map(|i| (format!("s{i}"), if i == 0 { vec![] } else { vec!["s0"] }))
        .collect();
    let refs: Vec<(&str, &[&str])> = steps
        .iter()
        .map(|(id, d)| (id.as_str(), d.as_slice()))
        .collect();
    let fx = Fx::new(&refs, "");
    for (i, (id, _)) in steps.iter().enumerate() {
        fx.built(id, &[("a.txt", &format!("{i}\n"))]);
    }
    let ledger = fx.ctx.ledger().unwrap();
    let runner = Runner::new(&fx.ctx, CheckRunner::default());
    runner.ensure_integration().unwrap();
    let ready = runner.sync(&ledger, &mut Summary::default()).unwrap();
    assert_eq!(ready.len(), 1, "only s0 has its deps landed");
    assert_eq!(
        ledger.reads(),
        1,
        "the pass folded the ledger more than once"
    );
}

/// Whether the test scratch dir can reflink, so the warm-worktree tests know to run.
fn scratch_can_reflink() -> bool {
    let t = tempdir("crew-queue-reflink-");
    fs::write(t.path().join("a"), "x").unwrap();
    crate::warm::clone_file(&t.path().join("a"), &t.path().join("b")).is_ok()
}

#[test]
fn a_new_worktree_starts_with_the_integration_worktrees_build_folders() {
    if !scratch_can_reflink() {
        eprintln!(
            "skipped: the scratch dir cannot reflink; set CREW_TEST_TMP to a btrfs or XFS dir"
        );
        return;
    }
    let fx = Fx::new(&[("s1", &[]), ("s2", &[])], "warm = [\"target\", \"out\"]");
    let integration = fx.ctx.integration_worktree();
    Runner::new(&fx.ctx, CheckRunner::default())
        .ensure_integration()
        .unwrap();
    fs::create_dir_all(integration.join("target/debug")).unwrap();
    fs::write(integration.join("target/debug/dep.rlib"), "built once").unwrap();
    fs::create_dir_all(integration.join("node_modules")).unwrap();
    fs::write(integration.join("node_modules/not-in-the-list"), "x").unwrap();

    let me = Some(u64::from(std::process::id()));
    let wt = worktrees::new(&fx.ctx, "s1", me).unwrap();
    assert_eq!(
        fs::read_to_string(wt.join("target/debug/dep.rlib")).unwrap(),
        "built once"
    );
    assert!(
        !wt.join("node_modules").exists(),
        "only the configured folders"
    );
    // Editing in the new tree does not touch the integration tree's copy.
    fs::write(wt.join("target/debug/dep.rlib"), "rebuilt").unwrap();
    assert_eq!(
        fs::read_to_string(integration.join("target/debug/dep.rlib")).unwrap(),
        "built once"
    );

    // A resumed worktree keeps what it has; --no-warm skips the clone.
    fs::remove_dir_all(wt.join("target")).unwrap();
    worktrees::new(&fx.ctx, "s1", me).unwrap();
    assert!(
        !wt.join("target").exists(),
        "an existing worktree is not warmed again"
    );
    let cold = worktrees::new_with(&fx.ctx, "s2", me, false).unwrap();
    assert!(!cold.join("target").exists());
}

#[test]
fn warm_names_that_leave_the_tree_are_a_config_error_and_the_default_list_is_the_usual_build_folders()
 {
    let fx = Fx::new(&[("s1", &[])], "");
    assert_eq!(
        fx.ctx.config.warm,
        ["target", "node_modules", ".venv"].map(PathBuf::from)
    );
    let toml = fs::read_to_string(fx.root.join("crew.toml")).unwrap();
    fs::write(fx.root.join("crew.toml"), format!("{toml}\n")).unwrap();
    let bad = toml.replace("[queue]\n", "[queue]\nwarm = [\"../outside\"]\n");
    fs::write(fx.root.join("crew.toml"), bad).unwrap();
    let err = Ctx::at(&fx.root).err().unwrap();
    assert!(
        err.to_string().contains("not a relative folder name"),
        "{err:#}"
    );
}
