//! The merge queue and worktree lifecycle for agent steps; state is git + the ledger + `.crew/queue.pb`.

pub mod check;
pub mod config;
pub mod ctx;
pub mod lock;
pub mod runner;
pub mod status;
pub mod store;
pub mod submit;
pub mod worktrees;

pub use ctx::Ctx;

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests;
