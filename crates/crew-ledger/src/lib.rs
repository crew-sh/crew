//! crew-ledger: typed plan (plan.toml) plus append-only run ledger (.crew/ledger.pb).
//! API stub: signatures are final, bodies are filled in next.

use std::path::{Path, PathBuf};

pub use crew_proto::ledger::{
    Approval, BudgetUse, Event, Landed, ScopeAll, Status, StepStatus, approval, event,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Msg(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    Asked,
    Extra,
    CoreDesign,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Review {
    OwnerBefore,
    OwnerAfter,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Model {
    Sonnet,
    Opus,
    Haiku,
    Fable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunKind {
    Build,
    Test,
    Harness,
    Game,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Budget {
    pub builds: Option<u32>,
    pub test_minutes: Option<u32>,
    pub harnesses: Option<u32>,
    pub games: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub id: String,
    pub title: String,
    pub kind: Kind,
    #[serde(default)]
    pub deps: Vec<String>,
    #[serde(default = "default_model")]
    pub model: Model,
    #[serde(default)]
    pub owns: Vec<String>,
    #[serde(default)]
    pub verify: Vec<String>,
    #[serde(default)]
    pub budget: Budget,
    pub review: Option<Review>,
    pub issue: Option<u64>,
}

fn default_model() -> Model {
    Model::Sonnet
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub request: String,
    pub steps: Vec<Step>,
}

/// Running totals of a step's recorded `BudgetUse`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Use {
    pub builds: u64,
    pub test_ms: u64,
    pub harnesses: u64,
    pub games: u64,
}

/// A step's current state: the fold of its events plus plan rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepState {
    pub status: Status,
    /// True when the step may leave PLANNED (no approval needed, or one was recorded).
    pub allowed: bool,
    pub landed: Option<Landed>,
    pub used: Use,
    pub by: String,
    pub detail: String,
    pub at_ms: u64,
}

pub struct Ledger {
    root: PathBuf,
    plan: Plan,
}

impl Ledger {
    /// `root` holds `crew.toml`.
    pub fn open(root: &Path) -> Result<Self, Error> {
        let _ = (root, &Self { root: PathBuf::new(), plan: Plan { request: String::new(), steps: vec![] } });
        Err(Error::Msg("not implemented".into()))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn plan(&self) -> &Plan {
        &self.plan
    }

    /// Steps whose deps all landed, that are PLANNED/APPROVED and allowed to start, in plan order.
    pub fn ready_steps(&self) -> Result<Vec<&Step>, Error> {
        Err(Error::Msg("not implemented".into()))
    }

    /// Appends one event (validated against the plan and its approval rules).
    pub fn record(&self, event: Event) -> Result<(), Error> {
        let _ = event;
        Err(Error::Msg("not implemented".into()))
    }

    pub fn state(&self, step_id: &str) -> Result<StepState, Error> {
        let _ = step_id;
        Err(Error::Msg("not implemented".into()))
    }
}
