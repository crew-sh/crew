use std::path::{Path, PathBuf};

use crate::branch_tip;
use crate::run::{Result, git};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    pub path: PathBuf,
    pub head: String,
    pub branch: Option<String>,
    pub locked: bool,
    /// Git considers the checkout gone (directory missing).
    pub prunable: bool,
}

pub fn worktree_list(repo: &Path) -> Result<Vec<Worktree>> {
    let text = git(repo, &["worktree", "list", "--porcelain"])?;
    let mut out = Vec::new();
    let mut cur: Option<Worktree> = None;
    for line in text.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            out.extend(cur.take());
            cur = Some(Worktree {
                path: PathBuf::from(p),
                head: String::new(),
                branch: None,
                locked: false,
                prunable: false,
            });
        } else if let Some(w) = cur.as_mut() {
            if let Some(h) = line.strip_prefix("HEAD ") {
                w.head = h.to_owned();
            } else if let Some(b) = line.strip_prefix("branch ") {
                w.branch = Some(b.strip_prefix("refs/heads/").unwrap_or(b).to_owned());
            } else if line == "locked" || line.starts_with("locked ") {
                w.locked = true;
            } else if line == "prunable" || line.starts_with("prunable ") {
                w.prunable = true;
            }
        }
    }
    out.extend(cur);
    Ok(out)
}

/// Add a worktree on `branch`, creating the branch from `base` when it does not exist yet.
pub fn worktree_add(repo: &Path, path: &Path, branch: &str, base: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let p = path.to_string_lossy();
    if branch_tip(repo, &format!("refs/heads/{branch}"))?.is_some() {
        git(repo, &["worktree", "add", &p, branch])?;
    } else {
        git(repo, &["worktree", "add", "-b", branch, &p, base])?;
    }
    Ok(())
}

/// Add a detached worktree at `rev` (for callers that move a ref themselves).
pub fn worktree_add_detached(repo: &Path, path: &Path, rev: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    git(
        repo,
        &["worktree", "add", "--detach", &path.to_string_lossy(), rev],
    )
    .map(|_| ())
}

/// Remove a registered worktree; dirty or locked ones need `force` (the double force git requires for locked).
pub fn worktree_remove(repo: &Path, path: &Path, force: bool) -> Result<()> {
    let registered = worktree_list(repo)?.into_iter().find(|w| w.path == path);
    let Some(w) = registered else {
        return Err(crate::Error::Refused(format!(
            "{} is not a registered worktree",
            path.display()
        )));
    };
    if w.prunable || !path.exists() {
        git(repo, &["worktree", "prune"])?;
        return Ok(());
    }
    let p = path.to_string_lossy();
    if force {
        git(repo, &["worktree", "remove", "--force", "--force", &p]).map(|_| ())
    } else {
        git(repo, &["worktree", "remove", &p]).map(|_| ())
    }
}
