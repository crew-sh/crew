use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use crew_git::MergeOutcome;
use crew_ledger::{Ledger, Status};

use crate::check::{CheckOutcome, CheckRunner};
use crate::ctx::Ctx;
use crate::lock::FileLock;
use crate::now_ms;
use crate::store::{Store, Submission};
use crate::submit::{record_landed, record_status, state_of};

const POLL: Duration = Duration::from_secs(3);

/// What one `run_once` pass did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Summary {
    pub landed: Vec<(String, String)>,
    pub failed: Vec<(String, String)>,
    pub batches: usize,
    pub push_errors: Vec<String>,
}

enum Fate {
    Landed(String),
    Failed(String),
}

pub struct Runner<'a> {
    ctx: &'a Ctx,
    check: CheckRunner,
}

impl<'a> Runner<'a> {
    pub fn new(ctx: &'a Ctx, check: CheckRunner) -> Self {
        Self { ctx, check }
    }

    /// Hold the single-runner lock for the life of the loop; `once` returns when nothing is ready.
    pub fn run(&self, once: bool) -> Result<Summary> {
        let lock_path = self.ctx.crew_dir().join("queue.lock");
        let Some(_lock) = FileLock::try_exclusive(&lock_path)? else {
            bail!("another queue runner holds {}", lock_path.display());
        };
        let mut total = Summary::default();
        loop {
            let s = self.run_pass()?;
            let idle = s.batches == 0;
            total.landed.extend(s.landed);
            total.failed.extend(s.failed);
            total.batches += s.batches;
            total.push_errors.extend(s.push_errors);
            if idle {
                if once {
                    return Ok(total);
                }
                std::thread::sleep(POLL);
            }
        }
    }

    /// Land ready batches until none is left; everything is recomputed from git and the ledger each round.
    fn run_pass(&self) -> Result<Summary> {
        let mut summary = Summary::default();
        let wt = self.ensure_integration()?;
        loop {
            let ledger = self.ctx.ledger()?;
            let ready = self.sync(&ledger, &mut summary)?;
            let Some(batch) = self.first_batch(&ready)? else {
                return Ok(summary);
            };
            summary.batches += 1;
            let batch_id = format!("b{}-{}", now_ms(), summary.batches);
            self.land(&ledger, &wt, &batch, &batch_id, &mut summary)?;
        }
    }

    fn integration_ref(&self) -> String {
        format!("refs/heads/{}", self.ctx.config.integration_branch)
    }

    fn integration_tip(&self) -> Result<String> {
        crew_git::branch_tip(&self.ctx.root, &self.integration_ref())?
            .context("integration branch vanished")
    }

    /// The queue's own detached worktree; the integration branch itself is moved with update-ref.
    fn ensure_integration(&self) -> Result<PathBuf> {
        let repo = &self.ctx.root;
        let cfg = &self.ctx.config;
        if crew_git::branch_tip(repo, &self.integration_ref())?.is_none() {
            let base = crew_git::branch_tip(repo, &cfg.base)?
                .with_context(|| format!("base `{}` does not exist", cfg.base))?;
            crew_git::update_ref(repo, &self.integration_ref(), &base, "")?;
        }
        if let Some(at) = crew_git::checked_out_at(repo, &cfg.integration_branch)? {
            bail!(
                "integration branch {} is checked out at {}; the queue moves it with update-ref, so check out another branch there",
                cfg.integration_branch,
                at.display()
            );
        }
        let wt = self.ctx.integration_worktree();
        let known = crew_git::worktree_list(repo)?
            .into_iter()
            .find(|w| w.path == wt);
        match known {
            Some(w) if !w.prunable && wt.exists() => {}
            Some(_) => {
                crew_git::worktree_remove(repo, &wt, true)?;
                crew_git::worktree_add_detached(repo, &wt, &self.integration_tip()?)?;
            }
            None => crew_git::worktree_add_detached(repo, &wt, &self.integration_tip()?)?,
        }
        Ok(wt)
    }

    /// Drop submissions that are done (landed, failed, merged already, branch gone); return the ready ones.
    fn sync(&self, ledger: &Ledger, summary: &mut Summary) -> Result<Vec<Submission>> {
        let store = Store::new(&self.ctx.crew_dir());
        let tip = self.integration_tip()?;
        let mut keep = Vec::new();
        let mut gone = Vec::new();
        for sub in store.load()?.submissions {
            let state = match state_of(ledger, &sub.step_id) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("queue: skipping {}: {e}", sub.step_id);
                    keep.push(sub);
                    continue;
                }
            };
            if matches!(state, Status::Landed | Status::Failed | Status::Dropped) {
                gone.push(sub.step_id.clone());
                continue;
            }
            let Some(branch_tip) =
                crew_git::branch_tip(&self.ctx.root, &format!("refs/heads/{}", sub.branch))?
            else {
                self.fail(
                    ledger,
                    &sub.step_id,
                    &format!("branch {} does not exist", sub.branch),
                    summary,
                )?;
                gone.push(sub.step_id.clone());
                continue;
            };
            if crew_git::is_ancestor(&self.ctx.root, &branch_tip, &tip)? {
                self.land_one(ledger, &sub.step_id, &branch_tip, "already-merged", summary)?;
                gone.push(sub.step_id.clone());
                continue;
            }
            if self.deps_landed(ledger, &sub.step_id) {
                keep.push(sub);
            }
        }
        if !gone.is_empty() {
            store.update(|f| f.submissions.retain(|s| !gone.contains(&s.step_id)))?;
        }
        Ok(keep)
    }

    fn deps_landed(&self, ledger: &Ledger, step: &str) -> bool {
        let Some(s) = ledger.plan().steps.iter().find(|s| s.id == step) else {
            return false;
        };
        s.deps
            .iter()
            .all(|d| state_of(ledger, d).is_ok_and(|st| st == Status::Landed))
    }

    /// Greedy: submission order, at most `max_batch`, no two branches touching the same file.
    fn first_batch(&self, ready: &[Submission]) -> Result<Option<Vec<Submission>>> {
        let tip = self.integration_tip()?;
        let mut batch: Vec<Submission> = Vec::new();
        let mut touched: Vec<String> = Vec::new();
        let mut ordered: Vec<&Submission> = ready.iter().collect();
        ordered.sort_by_key(|s| s.submitted_ms);
        for sub in ordered {
            if batch.len() >= self.ctx.config.max_batch {
                break;
            }
            let files = crew_git::changed_files(
                &self.ctx.root,
                &tip,
                &format!("refs/heads/{}", sub.branch),
            )?;
            if files.iter().any(|f| touched.contains(f)) {
                continue;
            }
            touched.extend(files);
            batch.push(sub.clone());
        }
        Ok((!batch.is_empty()).then_some(batch))
    }

    fn fail(&self, ledger: &Ledger, step: &str, detail: &str, summary: &mut Summary) -> Result<()> {
        record_status(ledger, step, Status::Failed, detail)?;
        eprintln!(
            "queue: FAILED {step}: {}",
            detail.lines().next().unwrap_or("")
        );
        summary.failed.push((step.into(), detail.into()));
        Ok(())
    }

    fn land_one(
        &self,
        ledger: &Ledger,
        step: &str,
        sha: &str,
        batch: &str,
        summary: &mut Summary,
    ) -> Result<()> {
        record_landed(ledger, step, sha, batch)?;
        println!("queue: landed {step} {}", &sha[..sha.len().min(12)]);
        summary.landed.push((step.into(), sha.into()));
        Ok(())
    }

    fn apply(
        &self,
        ledger: &Ledger,
        step: &str,
        fate: Fate,
        batch: &str,
        summary: &mut Summary,
    ) -> Result<()> {
        match fate {
            Fate::Landed(sha) => self.land_one(ledger, step, &sha, batch, summary)?,
            Fate::Failed(detail) => self.fail(ledger, step, &detail, summary)?,
        }
        Store::new(&self.ctx.crew_dir()).update(|f| f.submissions.retain(|s| s.step_id != step))
    }

    /// Merge `subs` onto the integration tip and check once; on red, bisect to the culprits and land the rest.
    fn land(
        &self,
        ledger: &Ledger,
        wt: &Path,
        subs: &[Submission],
        batch: &str,
        summary: &mut Summary,
    ) -> Result<()> {
        let repo = &self.ctx.root;
        let tip = self.integration_tip()?;
        crew_git::reset_hard(wt, &tip)?;
        crew_git::clean_untracked(wt)?;
        let mut merged: Vec<(&Submission, String)> = Vec::new();
        for sub in subs {
            let message = format!("Merge {} (step {})", sub.branch, sub.step_id);
            match crew_git::merge_no_ff(wt, &format!("refs/heads/{}", sub.branch), &message)? {
                MergeOutcome::Merged { sha } => merged.push((sub, sha)),
                MergeOutcome::AlreadyMerged => {
                    let sha = crew_git::branch_tip(repo, &format!("refs/heads/{}", sub.branch))?
                        .unwrap_or_default();
                    self.apply(ledger, &sub.step_id, Fate::Landed(sha), batch, summary)?;
                }
                MergeOutcome::Conflict { files } => {
                    let detail = format!(
                        "merge conflict with {} in: {}",
                        self.ctx.config.integration_branch,
                        files.join(", ")
                    );
                    self.apply(ledger, &sub.step_id, Fate::Failed(detail), batch, summary)?;
                }
            }
        }
        if merged.is_empty() {
            return Ok(());
        }
        match self.check.run_all(wt, &self.ctx.config.check)? {
            CheckOutcome::Green => {
                let head =
                    crew_git::branch_tip(wt, "HEAD")?.context("no HEAD in integration worktree")?;
                crew_git::update_ref(repo, &self.integration_ref(), &head, &tip)?;
                self.push(summary);
                for (sub, sha) in merged {
                    self.apply(ledger, &sub.step_id, Fate::Landed(sha), batch, summary)?;
                }
            }
            CheckOutcome::Red { command, tail } => {
                if let [(sub, _)] = merged.as_slice() {
                    let detail = format!("check failed: {command}\n{tail}");
                    return self.apply(ledger, &sub.step_id, Fate::Failed(detail), batch, summary);
                }
                let subs: Vec<Submission> = merged.into_iter().map(|(s, _)| s.clone()).collect();
                let (left, right) = subs.split_at(subs.len() / 2);
                self.land(ledger, wt, left, batch, summary)?;
                self.land(ledger, wt, right, batch, summary)?;
            }
        }
        Ok(())
    }

    fn push(&self, summary: &mut Summary) {
        let Some(p) = &self.ctx.config.push else {
            return;
        };
        if let Err(e) = crew_git::push(
            &self.ctx.root,
            &p.remote,
            &self.ctx.config.integration_branch,
            &p.branch,
        ) {
            eprintln!("queue: push failed: {e}");
            summary.push_errors.push(e.to_string());
        }
    }

    /// Run the `final` commands against the integration tip.
    pub fn run_final(&self) -> Result<CheckOutcome> {
        let wt = self.ensure_integration()?;
        crew_git::reset_hard(&wt, &self.integration_tip()?)?;
        self.check.run_all(&wt, &self.ctx.config.final_checks)
    }
}
