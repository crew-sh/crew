use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use crew_git::Salvaged;

use crate::ctx::Ctx;
use crate::now_ms;
use crate::store::{Store, WorktreeReg};

/// Start time (clock ticks since boot) of a live process, or None when it is gone.
fn proc_start(pid: u64) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = stat.rsplit_once(')')?.1;
    after_comm.split_whitespace().nth(19)?.parse().ok()
}

pub fn owner_alive(reg: &WorktreeReg) -> bool {
    reg.owner_pid != 0 && proc_start(reg.owner_pid) == Some(reg.owner_start)
}

/// Finished, or no live owner: such a worktree is never "taken".
pub fn is_dead(reg: &WorktreeReg) -> bool {
    reg.done || !owner_alive(reg)
}

fn register(ctx: &Ctx, step: &str, path: &Path, owner_pid: u64) -> Result<()> {
    let reg = WorktreeReg {
        step_id: step.into(),
        path: path.to_string_lossy().into_owned(),
        branch: ctx.step_branch(step),
        owner_pid,
        owner_start: proc_start(owner_pid).unwrap_or(0),
        created_ms: now_ms(),
        done: false,
    };
    Store::new(&ctx.crew_dir()).update(|f| {
        f.worktrees.retain(|w| w.step_id != step);
        f.worktrees.push(reg);
    })
}

fn parent_pid() -> u64 {
    // SAFETY: getppid has no preconditions.
    u64::try_from(unsafe { libc::getppid() }).unwrap_or(0)
}

/// Create (or take over, when its owner is dead) the step's worktree; defaults the owner to the caller's parent.
pub fn new(ctx: &Ctx, step: &str, owner_pid: Option<u64>) -> Result<PathBuf> {
    let owner = owner_pid.unwrap_or_else(parent_pid);
    let known = Store::new(&ctx.crew_dir())
        .load()?
        .worktrees
        .into_iter()
        .find(|w| w.step_id == step);
    if let Some(reg) = &known
        && !is_dead(reg)
        && reg.owner_pid != owner
    {
        bail!(
            "step {step} is taken: worktree {} is owned by live pid {}",
            reg.path,
            reg.owner_pid
        );
    }
    let path = ctx.worktree_root().join(step);
    let listed = crew_git::worktree_list(&ctx.root)?
        .into_iter()
        .find(|w| w.path == path);
    match listed {
        Some(w) if !w.prunable && path.exists() => {}
        Some(_) => {
            crew_git::worktree_remove(&ctx.root, &path, true)?;
            crew_git::worktree_add(&ctx.root, &path, &ctx.step_branch(step), &ctx.config.base)?;
        }
        None => crew_git::worktree_add(&ctx.root, &path, &ctx.step_branch(step), &ctx.config.base)?,
    }
    register(ctx, step, &path, owner)?;
    Ok(path)
}

/// Mark the step's worktree finished and drop its target dir; `gc` removes it later.
pub fn done(ctx: &Ctx, step: &str) -> Result<()> {
    let found = Store::new(&ctx.crew_dir()).update(|f| {
        let w = f.worktrees.iter_mut().find(|w| w.step_id == step)?;
        w.done = true;
        Some(PathBuf::from(&w.path))
    })?;
    let path = found.with_context(|| format!("no registered worktree for step {step}"))?;
    let target = path.join("target");
    if target.is_dir() {
        std::fs::remove_dir_all(&target).with_context(|| format!("remove {}", target.display()))?;
    }
    Ok(())
}

#[derive(Debug)]
pub struct SalvageRow {
    pub step_id: String,
    pub dest: PathBuf,
    pub salvaged: Salvaged,
}

fn keep_refs(ctx: &Ctx) -> Vec<String> {
    vec![format!("refs/heads/{}", ctx.config.integration_branch)]
}

/// Save the work of one step's worktree (any owner) or of every dead one into `to/<step>`.
pub fn salvage(
    ctx: &Ctx,
    step: Option<&str>,
    all_dead: bool,
    to: &Path,
) -> Result<Vec<SalvageRow>> {
    let regs = Store::new(&ctx.crew_dir()).load()?.worktrees;
    let picked: Vec<WorktreeReg> = match step {
        Some(s) => vec![
            regs.into_iter()
                .find(|w| w.step_id == s)
                .with_context(|| format!("no registered worktree for step {s}"))?,
        ],
        None if all_dead => regs.into_iter().filter(is_dead).collect(),
        None => bail!("name a step or pass --all-dead"),
    };
    let mut rows = Vec::new();
    for reg in picked {
        let path = PathBuf::from(&reg.path);
        if !path.exists() {
            continue;
        }
        let dest = to.join(&reg.step_id);
        let salvaged = crew_git::salvage(&path, &dest, &keep_refs(ctx))?;
        rows.push(SalvageRow {
            step_id: reg.step_id,
            dest,
            salvaged,
        });
    }
    Ok(rows)
}

#[derive(Debug)]
pub struct GcRow {
    pub step_id: String,
    pub removed: bool,
    pub salvage_dir: Option<PathBuf>,
}

/// Salvage then remove every dead registered worktree; a failed salvage keeps the worktree.
pub fn gc(ctx: &Ctx) -> Result<Vec<GcRow>> {
    let store = Store::new(&ctx.crew_dir());
    let dead: Vec<WorktreeReg> = store
        .load()?
        .worktrees
        .into_iter()
        .filter(is_dead)
        .collect();
    let mut rows = Vec::new();
    for reg in dead {
        let path = PathBuf::from(&reg.path);
        let mut salvage_dir = None;
        if path.exists() {
            let dest = ctx
                .crew_dir()
                .join("salvage")
                .join(format!("{}-{}", reg.step_id, now_ms()));
            let saved = crew_git::salvage(&path, &dest, &keep_refs(ctx));
            match saved {
                Ok(s) if s.is_empty() => {
                    let _ = std::fs::remove_dir_all(&dest);
                }
                Ok(_) => salvage_dir = Some(dest),
                Err(e) => {
                    eprintln!("queue: gc keeps {}: salvage failed: {e}", reg.step_id);
                    rows.push(GcRow {
                        step_id: reg.step_id,
                        removed: false,
                        salvage_dir: None,
                    });
                    continue;
                }
            }
        }
        let registered = crew_git::worktree_list(&ctx.root)?
            .iter()
            .any(|w| w.path == path);
        if registered {
            crew_git::worktree_remove(&ctx.root, &path, true)?;
        }
        let step = reg.step_id.clone();
        store.update(|f| f.worktrees.retain(|w| w.step_id != step))?;
        rows.push(GcRow {
            step_id: reg.step_id,
            removed: true,
            salvage_dir,
        });
    }
    Ok(rows)
}
