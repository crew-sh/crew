//! crew-ledger: the typed plan (plan.toml) and the append-only run ledger (.crew/ledger.pb).

mod check;
mod error;
mod glob;
mod ledger;
mod plan;
mod report;
mod store;

pub use check::{Violation, check, check_plan, over_budget};
pub use crew_proto::ledger::{
    Approval, BudgetUse, Event, Landed, ScopeAll, Status, StepStatus, approval, event,
};
pub use error::Error;
pub use ledger::{Ledger, Refusal, Snapshot, StepState, Use, events, now_ms};
pub use plan::{Budget, Kind, Model, Plan, RESERVED_SCOPE_ID, Review, RunKind, Step};
pub use report::{
    KindCount, Show, StepRow, Summary, render_markdown, render_show, show, summarize,
};

pub const STATUSES: [Status; 8] = [
    Status::Planned,
    Status::Approved,
    Status::Running,
    Status::Built,
    Status::Landed,
    Status::Failed,
    Status::Held,
    Status::Dropped,
];

pub fn status_name(s: Status) -> &'static str {
    match s {
        Status::Unspecified => "unspecified",
        Status::Planned => "planned",
        Status::Approved => "approved",
        Status::Running => "running",
        Status::Built => "built",
        Status::Landed => "landed",
        Status::Failed => "failed",
        Status::Held => "held",
        Status::Dropped => "dropped",
    }
}

pub fn parse_status(s: &str) -> Option<Status> {
    STATUSES.into_iter().find(|&x| status_name(x) == s)
}
