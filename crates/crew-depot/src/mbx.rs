//! Calls to the mbx program for read-only questions.
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::stats::{self, Lifetime};

/// `mbx explain --last` in `dir`: stdout then stderr, since mbx writes its notes to either.
pub fn explain_last(mbx: &Path, dir: &Path) -> Result<String> {
    let out = Command::new(mbx)
        .args(["explain", "--last"])
        .current_dir(dir)
        .env_remove("RUSTC_WRAPPER")
        .output()
        .with_context(|| format!("running {}", mbx.display()))?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    if !out.status.success() {
        bail!("mbx explain --last failed: {}", text.trim());
    }
    Ok(text)
}

/// mbx's lifetime totals; none when mbx fails or prints something unreadable.
pub fn lifetime(mbx: &Path) -> Option<Lifetime> {
    let out = Command::new(mbx).args(["stats", "--json"]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    stats::parse_lifetime(&String::from_utf8_lossy(&out.stdout)).ok()
}
