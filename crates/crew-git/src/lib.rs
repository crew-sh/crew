//! Typed wrappers over the `git` binary: worktrees, merges, ancestry, salvage.

mod run;
mod salvage;
mod worktree;

pub use run::{Error, Result};
pub use salvage::{Salvaged, salvage};
pub use worktree::{Worktree, worktree_add, worktree_add_detached, worktree_list, worktree_remove};

use std::path::{Path, PathBuf};

use run::{OutputExt, git, git_out};

/// Outcome of merging a branch into a worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeOutcome {
    Merged { sha: String },
    Conflict { files: Vec<String> },
    AlreadyMerged,
}

/// The commit a ref points at, or None when the ref does not exist.
pub fn branch_tip(repo: &Path, rev: &str) -> Result<Option<String>> {
    let out = git_out(
        repo,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{rev}^{{commit}}"),
        ],
    )?;
    Ok(out.status.success().then(|| out.stdout_trimmed()))
}

pub fn current_branch(repo: &Path) -> Result<Option<String>> {
    let out = git_out(repo, &["symbolic-ref", "--quiet", "--short", "HEAD"])?;
    Ok(out.status.success().then(|| out.stdout_trimmed()))
}

pub fn is_ancestor(repo: &Path, ancestor: &str, descendant: &str) -> Result<bool> {
    let out = git_out(repo, &["merge-base", "--is-ancestor", ancestor, descendant])?;
    match out.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(Error::failed(repo, &["merge-base", "--is-ancestor"], &out)),
    }
}

/// Files a branch changed relative to its merge base with `base`.
pub fn changed_files(repo: &Path, base: &str, branch: &str) -> Result<Vec<String>> {
    let text = git(
        repo,
        &["diff", "--name-only", "-z", &format!("{base}...{branch}")],
    )?;
    Ok(text
        .split('\0')
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect())
}

/// Merge `branch` into the branch checked out at `worktree` with a merge commit; a conflict is aborted.
pub fn merge_no_ff(worktree: &Path, branch: &str, message: &str) -> Result<MergeOutcome> {
    if is_ancestor(worktree, branch, "HEAD")? {
        return Ok(MergeOutcome::AlreadyMerged);
    }
    let out = git_out(
        worktree,
        &["merge", "--no-ff", "--no-edit", "-m", message, branch],
    )?;
    if out.status.success() {
        return Ok(MergeOutcome::Merged {
            sha: git(worktree, &["rev-parse", "HEAD"])?,
        });
    }
    let files = git(worktree, &["diff", "--name-only", "--diff-filter=U", "-z"])?;
    let files: Vec<String> = files
        .split('\0')
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    let abort = git_out(worktree, &["merge", "--abort"])?;
    if files.is_empty() {
        let _ = abort;
        return Err(Error::failed(worktree, &["merge", branch], &out));
    }
    Ok(MergeOutcome::Conflict { files })
}

/// Hard-reset a linked worktree; refuses the main worktree so a stray call cannot eat real work.
pub fn reset_hard(worktree: &Path, rev: &str) -> Result<()> {
    let git_dir = git(
        worktree,
        &["rev-parse", "--path-format=absolute", "--git-dir"],
    )?;
    let common = git(
        worktree,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    if git_dir == common {
        return Err(Error::Refused(format!(
            "{} is the main worktree",
            worktree.display()
        )));
    }
    git(worktree, &["reset", "--hard", rev]).map(|_| ())
}

/// True when `worktree` has tracked changes, untracked files, or an unfinished merge.
pub fn is_dirty(worktree: &Path) -> Result<bool> {
    Ok(!git(worktree, &["status", "--porcelain"])?.is_empty())
}

/// Remove untracked files (not ignored ones) from a linked worktree; refuses the main worktree.
pub fn clean_untracked(worktree: &Path) -> Result<()> {
    refuse_main(worktree)?;
    git(worktree, &["clean", "-fdq"]).map(|_| ())
}

/// Move `refname` to `new` only if it still points at `old` (empty `old` = must not exist yet).
pub fn update_ref(repo: &Path, refname: &str, new: &str, old: &str) -> Result<()> {
    git(repo, &["update-ref", refname, new, old]).map(|_| ())
}

/// Push `local` to `remote_branch` on `remote`.
pub fn push(repo: &Path, remote: &str, local: &str, remote_branch: &str) -> Result<()> {
    git(
        repo,
        &[
            "push",
            remote,
            &format!("{local}:refs/heads/{remote_branch}"),
        ],
    )
    .map(|_| ())
}

/// The worktree (if any) that has `branch` checked out.
pub fn checked_out_at(repo: &Path, branch: &str) -> Result<Option<PathBuf>> {
    Ok(worktree_list(repo)?
        .into_iter()
        .find(|w| w.branch.as_deref() == Some(branch))
        .map(|w| w.path))
}

fn refuse_main(worktree: &Path) -> Result<()> {
    let git_dir = git(
        worktree,
        &["rev-parse", "--path-format=absolute", "--git-dir"],
    )?;
    let common = git(
        worktree,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    if git_dir == common {
        return Err(Error::Refused(format!(
            "{} is the main worktree",
            worktree.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
