use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

use super::*;

/// Scratch dirs go under `CREW_TEST_TMP` when set (a big disk), else the system temp dir.
pub fn tmp() -> TempDir {
    let base = std::env::var_os("CREW_TEST_TMP").map_or_else(std::env::temp_dir, PathBuf::from);
    fs::create_dir_all(&base).unwrap();
    tempfile::Builder::new()
        .prefix("crew-git-")
        .tempdir_in(base)
        .unwrap()
}

pub fn sh(dir: &Path, args: &[&str]) -> String {
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

/// A repo on `main` with one commit.
fn repo() -> (TempDir, PathBuf) {
    let t = tmp();
    let r = t.path().join("repo");
    fs::create_dir(&r).unwrap();
    sh(&r, &["init", "-q", "-b", "main"]);
    fs::write(r.join("a.txt"), "one\n").unwrap();
    sh(&r, &["add", "."]);
    sh(&r, &["commit", "-qm", "init"]);
    (t, r)
}

fn commit_file(wt: &Path, name: &str, body: &str) {
    fs::write(wt.join(name), body).unwrap();
    sh(wt, &["add", name]);
    sh(wt, &["commit", "-qm", &format!("edit {name}")]);
}

#[test]
fn merge_lands_a_branch_and_is_idempotent() {
    let (t, r) = repo();
    let w = t.path().join("wt");
    worktree_add(&r, &w, "feat", "main").unwrap();
    commit_file(&w, "b.txt", "b\n");
    let outcome = merge_no_ff(&r, "feat", "land feat").unwrap();
    let MergeOutcome::Merged { sha } = outcome else {
        panic!("{outcome:?}")
    };
    assert_eq!(sha, sh(&r, &["rev-parse", "main"]));
    assert!(is_ancestor(&r, "feat", "main").unwrap());
    assert_eq!(
        merge_no_ff(&r, "feat", "again").unwrap(),
        MergeOutcome::AlreadyMerged
    );
    assert_eq!(sh(&r, &["log", "-1", "--format=%s"]), "land feat");
}

#[test]
fn merge_conflict_reports_files_and_leaves_the_tree_clean() {
    let (t, r) = repo();
    let (w1, w2) = (t.path().join("w1"), t.path().join("w2"));
    worktree_add(&r, &w1, "one", "main").unwrap();
    worktree_add(&r, &w2, "two", "main").unwrap();
    commit_file(&w1, "a.txt", "from one\n");
    commit_file(&w2, "a.txt", "from two\n");
    assert!(matches!(
        merge_no_ff(&r, "one", "m").unwrap(),
        MergeOutcome::Merged { .. }
    ));
    let tip = sh(&r, &["rev-parse", "main"]);
    assert_eq!(
        merge_no_ff(&r, "two", "m").unwrap(),
        MergeOutcome::Conflict {
            files: vec!["a.txt".into()]
        }
    );
    assert_eq!(sh(&r, &["rev-parse", "main"]), tip);
    assert!(!is_dirty(&r).unwrap());
}

#[test]
fn changed_files_are_relative_to_the_merge_base() {
    let (t, r) = repo();
    let w = t.path().join("wt");
    worktree_add(&r, &w, "feat", "main").unwrap();
    commit_file(&w, "b.txt", "b\n");
    commit_file(&r, "c.txt", "c\n");
    assert_eq!(
        changed_files(&r, "main", "feat").unwrap(),
        vec!["b.txt".to_owned()]
    );
}

#[test]
fn branch_tip_is_none_for_a_missing_ref() {
    let (_t, r) = repo();
    assert_eq!(branch_tip(&r, "nope").unwrap(), None);
    assert!(branch_tip(&r, "main").unwrap().is_some());
}

#[test]
fn reset_hard_refuses_the_main_worktree_and_resets_linked_ones() {
    let (t, r) = repo();
    let w = t.path().join("wt");
    worktree_add(&r, &w, "feat", "main").unwrap();
    commit_file(&w, "b.txt", "b\n");
    assert!(matches!(reset_hard(&r, "HEAD~0"), Err(Error::Refused(_))));
    reset_hard(&w, "main").unwrap();
    assert!(!w.join("b.txt").exists());
}

#[test]
fn worktree_list_add_remove_and_dead_prune() {
    let (t, r) = repo();
    let w = t.path().join("deep/wt");
    worktree_add(&r, &w, "feat", "main").unwrap();
    let listed = worktree_list(&r).unwrap();
    let mine = listed.iter().find(|x| x.path == w).unwrap();
    assert_eq!(mine.branch.as_deref(), Some("feat"));
    // dirty needs force
    fs::write(w.join("dirty.txt"), "x").unwrap();
    assert!(worktree_remove(&r, &w, false).is_err());
    worktree_remove(&r, &w, true).unwrap();
    assert_eq!(worktree_list(&r).unwrap().len(), 1);

    // a worktree whose directory vanished is pruned
    worktree_add(&r, &w, "feat", "main").unwrap();
    fs::remove_dir_all(&w).unwrap();
    assert!(
        worktree_list(&r)
            .unwrap()
            .iter()
            .any(|x| x.path == w && x.prunable)
    );
    worktree_remove(&r, &w, false).unwrap();
    assert_eq!(worktree_list(&r).unwrap().len(), 1);
    assert!(matches!(
        worktree_remove(&r, &w, true),
        Err(Error::Refused(_))
    ));
}

#[test]
fn salvage_keeps_diff_untracked_files_and_unlanded_commits_only() {
    let (t, r) = repo();
    let w = t.path().join("wt");
    worktree_add(&r, &w, "feat", "main").unwrap();
    commit_file(&w, "kept.txt", "committed\n");
    fs::write(w.join("a.txt"), "changed\n").unwrap();
    fs::write(w.join("new.txt"), "untracked\n").unwrap();
    fs::write(w.join(".gitignore"), "target/\n").unwrap();
    fs::create_dir(w.join("target")).unwrap();
    fs::write(w.join("target/junk"), "ignored").unwrap();

    let dest = t.path().join("out");
    let s = salvage(&w, &dest, &["main".to_owned()]).unwrap();
    assert!(
        fs::read_to_string(s.diff.unwrap())
            .unwrap()
            .contains("+changed")
    );
    let tar = Command::new("tar")
        .arg("-tzf")
        .arg(s.untracked.unwrap())
        .output()
        .unwrap();
    let names = String::from_utf8_lossy(&tar.stdout).into_owned();
    assert!(names.contains("new.txt"), "{names}");
    assert!(!names.contains("target"), "{names}");
    assert_eq!(s.patches.len(), 1);
    assert!(
        fs::read_to_string(&s.patches[0])
            .unwrap()
            .contains("kept.txt")
    );

    // once merged into the kept ref, no patch is left
    merge_no_ff(&r, "feat", "land").unwrap();
    let again = salvage(&w, &t.path().join("out2"), &["main".to_owned()]).unwrap();
    assert!(again.patches.is_empty());
}

#[test]
fn salvage_of_a_clean_worktree_is_empty() {
    let (t, r) = repo();
    let w = t.path().join("wt");
    worktree_add(&r, &w, "feat", "main").unwrap();
    let s = salvage(&w, &t.path().join("out"), &["main".to_owned()]).unwrap();
    assert!(s.is_empty());
}

#[test]
fn update_ref_is_compare_and_swap_and_checked_out_at_finds_the_holder() {
    let (t, r) = repo();
    let w = t.path().join("wt");
    worktree_add(&r, &w, "feat", "main").unwrap();
    commit_file(&w, "b.txt", "b\n");
    let (old, new) = (
        sh(&r, &["rev-parse", "main"]),
        sh(&r, &["rev-parse", "feat"]),
    );
    assert!(update_ref(&r, "refs/heads/main", &new, &"0".repeat(40)).is_err());
    update_ref(&r, "refs/heads/main", &new, &old).unwrap();
    assert_eq!(sh(&r, &["rev-parse", "main"]), new);
    assert_eq!(checked_out_at(&r, "feat").unwrap(), Some(w));
    assert_eq!(checked_out_at(&r, "nope").unwrap(), None);
}

#[test]
fn clean_untracked_keeps_ignored_files_and_refuses_main() {
    let (t, r) = repo();
    let d = t.path().join("det");
    worktree_add_detached(&r, &d, "main").unwrap();
    fs::write(d.join(".gitignore"), "target/\n").unwrap();
    fs::create_dir(d.join("target")).unwrap();
    fs::write(d.join("target/x"), "1").unwrap();
    fs::write(d.join("stray"), "1").unwrap();
    clean_untracked(&d).unwrap();
    assert!(!d.join("stray").exists() && d.join("target/x").exists());
    assert!(clean_untracked(&r).is_err());
}

#[test]
fn salvage_with_no_integration_ref_keeps_every_unpushed_commit() {
    let (t, r) = repo();
    let w = t.path().join("wt");
    worktree_add(&r, &w, "feat", "main").unwrap();
    commit_file(&w, "kept.txt", "committed\n");
    let s = salvage(
        &w,
        &t.path().join("out"),
        &["refs/heads/missing".to_owned()],
    )
    .unwrap();
    assert_eq!(s.patches.len(), 2);
}
