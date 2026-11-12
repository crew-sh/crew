use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use crew_ledger::Ledger;

use crate::config::{self, QueueConfig};

/// Everything a queue command needs to know about where it runs.
pub struct Ctx {
    /// The main worktree directory holding `crew.toml`.
    pub root: PathBuf,
    pub config: QueueConfig,
}

impl Ctx {
    /// Find `crew.toml` upward from `start` and map it onto the main worktree, so linked worktrees share one queue.
    pub fn discover(start: &Path) -> Result<Self> {
        let start = start
            .canonicalize()
            .with_context(|| format!("resolve {}", start.display()))?;
        let found = start
            .ancestors()
            .find(|d| d.join("crew.toml").is_file())
            .with_context(|| format!("no crew.toml above {}", start.display()))?
            .to_owned();
        let trees = crew_git::worktree_list(&found).context("list worktrees")?;
        let Some(main) = trees.first().map(|w| w.path.clone()) else {
            bail!("{} is not in a git repository", found.display());
        };
        let holder = trees
            .iter()
            .filter(|w| found.starts_with(&w.path))
            .max_by_key(|w| w.path.as_os_str().len());
        let root = match holder {
            Some(w) => main.join(found.strip_prefix(&w.path).unwrap_or(Path::new(""))),
            None => found,
        };
        Self::at(&root)
    }

    pub fn at(root: &Path) -> Result<Self> {
        Ok(Self {
            root: root.to_owned(),
            config: config::load(root)?,
        })
    }

    pub fn crew_dir(&self) -> PathBuf {
        self.root.join(".crew")
    }

    pub fn worktree_root(&self) -> PathBuf {
        match &self.config.worktree_root {
            Some(p) if p.is_absolute() => p.clone(),
            Some(p) => self.root.join(p),
            None => self.crew_dir().join("wt"),
        }
    }

    pub fn integration_worktree(&self) -> PathBuf {
        self.worktree_root().join(".integration")
    }

    pub fn step_branch(&self, step: &str) -> String {
        format!("{}{step}", self.config.branch_prefix)
    }

    /// A fresh ledger view; reopen per round so nothing cached is trusted.
    pub fn ledger(&self) -> Result<Ledger> {
        Ledger::open(&self.root).map_err(|e| anyhow::anyhow!("ledger: {e}"))
    }
}
