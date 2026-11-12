use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::branch_tip;
use crate::run::{Error, Result, git, git_out};

/// What salvage wrote into the destination folder.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Salvaged {
    /// `tracked.diff`: tracked changes against HEAD, when there were any.
    pub diff: Option<PathBuf>,
    /// `untracked.tar.gz`: untracked files, minus what .gitignore hides.
    pub untracked: Option<PathBuf>,
    /// `patches/*.patch`: commits not reachable from `keep_refs` or any remote.
    pub patches: Vec<PathBuf>,
}

impl Salvaged {
    pub fn is_empty(&self) -> bool {
        self.diff.is_none() && self.untracked.is_none() && self.patches.is_empty()
    }
}

/// Save a worktree's unsaved work into `dest`: tracked diff, untracked tarball, unlanded commits as patches.
pub fn salvage(worktree: &Path, dest: &Path, keep_refs: &[String]) -> Result<Salvaged> {
    std::fs::create_dir_all(dest)?;
    let mut out = Salvaged::default();

    let diff = git_out(worktree, &["diff", "--binary", "HEAD"])?;
    if diff.status.success() && !diff.stdout.is_empty() {
        let p = dest.join("tracked.diff");
        std::fs::write(&p, &diff.stdout)?;
        out.diff = Some(p);
    }

    let untracked = git_out(
        worktree,
        &["ls-files", "--others", "--exclude-standard", "-z"],
    )?;
    if !untracked.stdout.is_empty() {
        let p = dest.join("untracked.tar.gz");
        let mut tar = Command::new("tar")
            .arg("-C")
            .arg(worktree)
            .args(["--null", "-T", "-", "-czf"])
            .arg(&p)
            .stdin(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        use std::io::Write;
        tar.stdin
            .take()
            .expect("piped")
            .write_all(&untracked.stdout)?;
        let res = tar.wait_with_output()?;
        if !res.status.success() {
            return Err(Error::Refused(format!(
                "tar failed: {}",
                String::from_utf8_lossy(&res.stderr).trim()
            )));
        }
        out.untracked = Some(p);
    }

    let mut args: Vec<String> = ["format-patch", "-o"].map(str::to_owned).into();
    let patch_dir = dest.join("patches");
    args.push(patch_dir.to_string_lossy().into_owned());
    args.push("HEAD".into());
    args.push("--not".into());
    args.push("--remotes".into());
    for r in keep_refs {
        if branch_tip(worktree, r)?.is_some() {
            args.push(r.clone());
        }
    }
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let listed = git(worktree, &refs)?;
    out.patches = listed
        .lines()
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .collect();
    Ok(out)
}
