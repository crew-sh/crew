use anyhow::{Result, bail};
use crew_ledger::{Event, Landed, Ledger, Status, StepStatus, event};

use crate::ctx::Ctx;
use crate::now_ms;
use crate::store::{Store, Submission};

const BY: &str = "queue";

pub fn record_status(ledger: &Ledger, step: &str, status: Status, detail: &str) -> Result<()> {
    let body = StepStatus {
        step_id: step.into(),
        status: status.into(),
        at_ms: now_ms(),
        by: BY.into(),
        detail: detail.into(),
    };
    ledger
        .record(Event {
            body: Some(event::Body::Status(body)),
        })
        .map_err(|e| anyhow::anyhow!("ledger: {e}"))
}

pub fn record_landed(ledger: &Ledger, step: &str, sha: &str, batch: &str) -> Result<()> {
    let body = Landed {
        step_id: step.into(),
        sha: sha.into(),
        batch_id: batch.into(),
        at_ms: now_ms(),
    };
    ledger
        .record(Event {
            body: Some(event::Body::Landed(body)),
        })
        .map_err(|e| anyhow::anyhow!("ledger: {e}"))
}

pub fn state_of(ledger: &Ledger, step: &str) -> Result<Status> {
    Ok(ledger
        .state(step)
        .map_err(|e| anyhow::anyhow!("ledger: {e}"))?
        .status)
}

/// Record the step BUILT and queue its branch; resubmitting replaces the earlier entry.
pub fn submit(ctx: &Ctx, branch: &str, step: &str) -> Result<()> {
    if crew_git::branch_tip(&ctx.root, &format!("refs/heads/{branch}"))?.is_none() {
        bail!("branch {branch} does not exist");
    }
    let ledger = ctx.ledger()?;
    if state_of(&ledger, step)? == Status::Landed {
        bail!("step {step} already landed");
    }
    record_status(&ledger, step, Status::Built, &format!("submitted {branch}"))?;
    let sub = Submission {
        step_id: step.into(),
        branch: branch.into(),
        submitted_ms: now_ms(),
    };
    Store::new(&ctx.crew_dir()).update(|f| {
        f.submissions.retain(|s| s.step_id != step);
        f.submissions.push(sub);
    })
}
