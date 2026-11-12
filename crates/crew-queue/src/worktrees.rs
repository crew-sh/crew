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

fn new_reg(ctx: &Ctx, step: &str, path: &Path, owner_pid: u64) -> WorktreeReg {
    WorktreeReg {
        step_id: step.into(),
        path: path.to_string_lossy().into_owned(),
        branch: ctx.step_branch(step),
        owner_pid,
        owner_start: proc_start(owner_pid).unwrap_or(0),
        created_ms: now_ms(),
        done: false,
    }
}

fn parent_pid() -> u64 {
    // SAFETY: getppid has no preconditions.
    u64::try_from(unsafe { libc::getppid() }).unwrap_or(0)
}

fn session_leader() -> u64 {
    // SAFETY: getsid(0) asks about the calling process.
    u64::try_from(unsafe { libc::getsid(0) }).unwrap_or(0)
}

/// `(parent pid, command name)` of a process.
fn parent_and_comm(pid: u64) -> Option<(u64, String)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let open = stat.find('(')?;
    let close = stat.rfind(')')?;
    let ppid = stat
        .get(close + 1..)?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()?;
    Some((ppid, stat[open + 1..close].to_string()))
}

/// Who owns a new worktree when nobody says: the agent session above the caller, else the session leader.
/// The caller's own parent is often a `sh -c` that is gone a moment later.
pub fn default_owner(chain: &[(u64, String)], session: u64) -> u64 {
    chain
        .iter()
        .find(|(_, comm)| matches!(comm.as_str(), "claude" | "codex"))
        .map_or(session, |(pid, _)| *pid)
}

fn ancestors() -> Vec<(u64, String)> {
    let mut out = Vec::new();
    let mut pid = parent_pid();
    while pid > 1 && out.len() < 64 {
        let Some((ppid, comm)) = parent_and_comm(pid) else {
            break;
        };
        out.push((pid, comm));
        pid = ppid;
    }
    out
}

/// Create (or take over, when its owner is dead) the step's worktree. The whole decision runs under the
/// queue's state lock, so two callers cannot both believe they own the step.
pub fn new(ctx: &Ctx, step: &str, owner_pid: Option<u64>) -> Result<PathBuf> {
    if !crew_ledger::valid_id(step) {
        bail!(
            "`{step}` is not a usable step id (letters, digits, `.`, `_`, `-`; not starting with `.` or `-`)"
        );
    }
    if ctx.ledger()?.plan().step(step).is_none() {
        bail!("step `{step}` is not in the plan");
    }
    let owner = owner_pid.unwrap_or_else(|| {
        let session = session_leader();
        let owner = default_owner(&ancestors(), session);
        if owner == 0 { parent_pid() } else { owner }
    });
    let path = ctx.worktree_root().join(step);
    Store::new(&ctx.crew_dir()).update(|f| -> Result<()> {
        if let Some(reg) = f.worktrees.iter().find(|w| w.step_id == step)
            && !is_dead(reg)
            && reg.owner_pid != owner
        {
            bail!(
                "step {step} is taken: worktree {} is owned by live pid {}",
                reg.path,
                reg.owner_pid
            );
        }
        let listed = crew_git::worktree_list(&ctx.root)?
            .into_iter()
            .find(|w| w.path == path);
        match listed {
            Some(w) if !w.prunable && path.exists() => {}
            Some(_) => {
                crew_git::worktree_remove(&ctx.root, &path, true)?;
                crew_git::worktree_add(&ctx.root, &path, &ctx.step_branch(step), &ctx.config.base)?;
            }
            None => {
                crew_git::worktree_add(&ctx.root, &path, &ctx.step_branch(step), &ctx.config.base)?;
            }
        }
        f.worktrees.retain(|w| w.step_id != step);
        f.worktrees.push(new_reg(ctx, step, &path, owner));
        Ok(())
    })??;
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

/// Pids of other processes whose working directory is inside `dir`.
fn pids_working_in(dir: &Path) -> Vec<u32> {
    let me = std::process::id();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| *pid != me)
        .filter(|pid| {
            std::fs::read_link(format!("/proc/{pid}/cwd")).is_ok_and(|cwd| cwd.starts_with(dir))
        })
        .collect()
}

/// Salvage then remove every dead registered worktree; a failed salvage keeps the worktree, and so does a
/// process still working in it or an owner that came back. Ignored files (.gitignore) are not saved.
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
        if let Some(pid) = pids_working_in(&path).first() {
            eprintln!(
                "queue: gc keeps {}: pid {pid} is working in it",
                reg.step_id
            );
            rows.push(GcRow {
                step_id: reg.step_id,
                removed: false,
                salvage_dir: None,
            });
            continue;
        }
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
        // Salvage took a while; the step may have been taken over meanwhile.
        let still_dead = store
            .load()?
            .worktrees
            .iter()
            .find(|w| w.step_id == reg.step_id)
            .is_some_and(|now| *now == reg || is_dead(now) && now.owner_pid == reg.owner_pid);
        if !still_dead {
            eprintln!("queue: gc keeps {}: it was taken over", reg.step_id);
            continue;
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
