use std::env;
use std::path::{Path, PathBuf};

fn uid() -> u32 {
    unsafe { libc::getuid() }
}

fn home() -> PathBuf {
    env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from)
}

fn xdg(var: &str, fallback: impl FnOnce() -> PathBuf) -> PathBuf {
    match env::var_os(var) {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => fallback(),
    }
}

/// `CREW_SOCKET` overrides the default for tests and side-by-side daemons.
pub fn socket_path() -> PathBuf {
    if let Some(p) = env::var_os("CREW_SOCKET") {
        return PathBuf::from(p);
    }
    xdg("XDG_RUNTIME_DIR", || {
        PathBuf::from(format!("/run/user/{}", uid()))
    })
    .join("crew/usher.sock")
}

pub fn state_dir() -> PathBuf {
    xdg("XDG_STATE_HOME", || home().join(".local/state")).join("crew")
}

pub fn config_dir() -> PathBuf {
    xdg("XDG_CONFIG_HOME", || home().join(".config")).join("crew")
}

/// The nearest enclosing directory holding `.git` (a checkout or a worktree).
fn checkout_dir(cwd: &Path) -> Option<PathBuf> {
    cwd.ancestors()
        .find(|d| d.join(".git").exists())
        .map(Path::to_path_buf)
}

fn absolute_from(base: &Path, p: &str) -> PathBuf {
    let p = Path::new(p.trim());
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    };
    std::fs::canonicalize(&joined).unwrap_or(joined)
}

/// A linked worktree's `.git` file names its git dir, whose `commondir` names the repository's own.
fn common_git_dir(checkout: &Path) -> Option<PathBuf> {
    let file = std::fs::read_to_string(checkout.join(".git")).ok()?;
    let gitdir = absolute_from(checkout, file.trim().strip_prefix("gitdir:")?);
    let common = std::fs::read_to_string(gitdir.join("commondir")).ok()?;
    Some(absolute_from(&gitdir, &common))
}

/// The repository's main checkout: the git toplevel of `cwd`, or for a linked worktree the checkout it was added from.
pub fn repo_root(cwd: &Path) -> Option<PathBuf> {
    let checkout = checkout_dir(cwd)?;
    let Some(common) = common_git_dir(&checkout) else {
        return Some(checkout);
    };
    if common.file_name().is_some_and(|n| n == ".git") {
        Some(common.parent().map_or(checkout, Path::to_path_buf))
    } else {
        Some(common)
    }
}

/// Basename of the repository of `cwd` (worktrees count as their repo), else of `cwd` itself.
pub fn project_of(cwd: &Path) -> String {
    let top = repo_root(cwd);
    let dir = top.as_deref().unwrap_or(cwd);
    dir.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .trim_end_matches(".git")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn worktree(repo: &Path, wt: &Path, name: &str, relative: bool) {
        let gitdir = repo.join(".git/worktrees").join(name);
        fs::create_dir_all(&gitdir).unwrap();
        fs::create_dir_all(wt).unwrap();
        fs::write(
            gitdir.join("commondir"),
            if relative { "../.." } else { "" },
        )
        .unwrap();
        if !relative {
            fs::write(
                gitdir.join("commondir"),
                repo.join(".git").display().to_string(),
            )
            .unwrap();
        }
        fs::write(wt.join(".git"), format!("gitdir: {}\n", gitdir.display())).unwrap();
    }

    #[test]
    fn project_is_the_git_toplevel_basename() {
        let dir = tempfile::tempdir().unwrap();
        let top = dir.path().join("horizonmc");
        let deep = top.join("a/b");
        fs::create_dir_all(&deep).unwrap();
        fs::create_dir(top.join(".git")).unwrap();
        assert_eq!(repo_root(&deep), Some(top.clone()));
        assert_eq!(project_of(&deep), "horizonmc");
        let plain = dir.path().join("plain");
        fs::create_dir(&plain).unwrap();
        assert_eq!(project_of(&plain), "plain");
    }

    #[test]
    fn a_linked_worktree_belongs_to_the_repo_it_was_added_from() {
        let dir = tempfile::tempdir().unwrap();
        let repo = fs::canonicalize(dir.path()).unwrap().join("horizonmc");
        fs::create_dir_all(repo.join(".git")).unwrap();
        // The layout crew-queue uses: a worktree inside the repo, and one far away.
        let inside = repo.join(".crew/wt/.integration");
        let outside = dir.path().join("elsewhere/feature-x");
        worktree(&repo, &inside, "integration", false);
        worktree(&repo, &outside, "feature-x", true);
        for cwd in [inside.join("crates/a"), outside.clone()] {
            fs::create_dir_all(&cwd).unwrap();
            assert_eq!(project_of(&cwd), "horizonmc", "{}", cwd.display());
            assert_eq!(repo_root(&cwd), Some(repo.clone()));
        }
    }

    #[test]
    fn a_submodule_and_a_bare_repo_keep_their_own_names() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("super/vendor/lib");
        fs::create_dir_all(&sub).unwrap();
        let modules = dir.path().join("super/.git/modules/lib");
        fs::create_dir_all(&modules).unwrap();
        fs::write(sub.join(".git"), format!("gitdir: {}", modules.display())).unwrap();
        assert_eq!(project_of(&sub), "lib");
        let bare = dir.path().join("server.git");
        let wt = dir.path().join("wt");
        let gitdir = bare.join("worktrees/wt");
        fs::create_dir_all(&gitdir).unwrap();
        fs::create_dir_all(&wt).unwrap();
        fs::write(gitdir.join("commondir"), "../..").unwrap();
        fs::write(wt.join(".git"), format!("gitdir: {}", gitdir.display())).unwrap();
        assert_eq!(project_of(&wt), "server");
    }
}
