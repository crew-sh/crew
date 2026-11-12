use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PushConfig {
    pub remote: String,
    pub branch: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueueConfig {
    pub integration_branch: String,
    pub base: String,
    /// Defaults to `<repo>/.crew/wt`.
    pub worktree_root: Option<PathBuf>,
    #[serde(default)]
    pub check: Vec<String>,
    #[serde(default, rename = "final")]
    pub final_checks: Vec<String>,
    pub push: Option<PushConfig>,
    #[serde(default = "default_max_batch")]
    pub max_batch: usize,
    /// Step branches are `<prefix><step id>`.
    #[serde(default = "default_prefix")]
    pub branch_prefix: String,
}

fn default_max_batch() -> usize {
    8
}

fn default_prefix() -> String {
    "crew/".into()
}

/// Only `[queue]` is ours; `[plan]` and anything else in crew.toml belongs to other tools.
#[derive(Deserialize)]
struct CrewToml {
    queue: Option<QueueConfig>,
}

pub fn load(root: &Path) -> Result<QueueConfig> {
    let path = root.join("crew.toml");
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let parsed: CrewToml =
        toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    let mut q = parsed
        .queue
        .with_context(|| format!("{} has no [queue] table", path.display()))?;
    q.max_batch = q.max_batch.max(1);
    Ok(q)
}
