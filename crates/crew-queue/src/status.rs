use std::fmt::Write;

use anyhow::Result;
use crew_ledger::{Kind, Status};
use serde::Serialize;

use crate::ctx::Ctx;
use crate::lock::FileLock;
use crate::store::Store;
use crate::submit::state_of;
use crate::worktrees::is_dead;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    Queued,
    WaitingOnDeps,
    Landed,
    Failed,
}

#[derive(Debug, Serialize)]
pub struct StepRow {
    pub step: String,
    pub kind: Kind,
    pub phase: Phase,
    pub branch: Option<String>,
    /// The landed sha, or the failure text.
    pub detail: String,
}

#[derive(Debug, Serialize)]
pub struct WorktreeRow {
    pub step: String,
    pub path: String,
    pub owner_pid: u64,
    pub dead: bool,
    pub done: bool,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub integration_branch: String,
    pub integration_tip: Option<String>,
    pub runner_active: bool,
    pub steps: Vec<StepRow>,
    pub worktrees: Vec<WorktreeRow>,
}

/// Recomputed from git, the ledger and the queue file on every call.
pub fn report(ctx: &Ctx) -> Result<Report> {
    let ledger = ctx.ledger()?;
    let file = Store::new(&ctx.crew_dir()).load()?;
    let tip = crew_git::branch_tip(
        &ctx.root,
        &format!("refs/heads/{}", ctx.config.integration_branch),
    )?;
    let lock_path = ctx.crew_dir().join("queue.lock");
    let runner_active = FileLock::try_exclusive(&lock_path)?.is_none();
    let mut steps = Vec::new();
    for step in &ledger.plan().steps {
        let queued = file.submissions.iter().find(|s| s.step_id == step.id);
        let state = ledger
            .state(&step.id)
            .map_err(|e| anyhow::anyhow!("ledger: {e}"))?;
        let (phase, detail) = match state.status {
            Status::Landed => (
                Phase::Landed,
                state
                    .landed
                    .as_ref()
                    .map_or_else(String::new, |l| l.sha.clone()),
            ),
            Status::Failed => (Phase::Failed, state.detail.clone()),
            _ if queued.is_some() => {
                let waiting = step
                    .deps
                    .iter()
                    .any(|d| state_of(&ledger, d).is_ok_and(|s| s != Status::Landed));
                (
                    if waiting {
                        Phase::WaitingOnDeps
                    } else {
                        Phase::Queued
                    },
                    String::new(),
                )
            }
            _ => continue,
        };
        steps.push(StepRow {
            step: step.id.clone(),
            kind: step.kind,
            phase,
            branch: queued.map(|q| q.branch.clone()),
            detail,
        });
    }
    let worktrees = file
        .worktrees
        .iter()
        .map(|w| WorktreeRow {
            step: w.step_id.clone(),
            path: w.path.clone(),
            owner_pid: w.owner_pid,
            dead: is_dead(w),
            done: w.done,
        })
        .collect();
    Ok(Report {
        integration_branch: ctx.config.integration_branch.clone(),
        integration_tip: tip,
        runner_active,
        steps,
        worktrees,
    })
}

pub fn render(r: &Report) -> String {
    let mut out = String::new();
    let tip = r
        .integration_tip
        .as_deref()
        .map_or("(none)", |t| &t[..t.len().min(12)]);
    let _ = writeln!(
        out,
        "integration {} at {tip}; runner {}",
        r.integration_branch,
        if r.runner_active { "active" } else { "idle" }
    );
    for kind in [Kind::Asked, Kind::Extra, Kind::CoreDesign] {
        let rows: Vec<&StepRow> = r.steps.iter().filter(|s| s.kind == kind).collect();
        if rows.is_empty() {
            continue;
        }
        let _ = writeln!(out, "{kind:?}:");
        for s in rows {
            let first = s.detail.lines().next().unwrap_or("");
            let _ = writeln!(out, "  {:<24} {:?} {first}", s.step, s.phase);
        }
    }
    if !r.worktrees.is_empty() {
        let _ = writeln!(out, "worktrees:");
    }
    for w in &r.worktrees {
        let state = if w.done {
            "done"
        } else if w.dead {
            "dead owner"
        } else {
            "owned"
        };
        let _ = writeln!(
            out,
            "  {:<24} {state} (pid {}) {}",
            w.step, w.owner_pid, w.path
        );
    }
    out
}
