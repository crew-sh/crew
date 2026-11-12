use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::plan::{self, Plan, RunKind, Step};
use crate::{Error, store};
use crew_proto::ledger::{Approval, BudgetUse, Event, Landed, Status, StepStatus, approval, event};

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// Constructors for the four event kinds; `at_ms` is stamped when recorded.
pub mod events {
    use super::*;

    pub fn status(step_id: &str, status: Status, by: &str, detail: &str) -> Event {
        Event {
            body: Some(event::Body::Status(StepStatus {
                step_id: step_id.into(),
                status: status as i32,
                at_ms: 0,
                by: by.into(),
                detail: detail.into(),
            })),
        }
    }

    pub fn landed(step_id: &str, sha: &str, batch_id: &str) -> Event {
        Event {
            body: Some(event::Body::Landed(Landed {
                step_id: step_id.into(),
                sha: sha.into(),
                batch_id: batch_id.into(),
                at_ms: 0,
            })),
        }
    }

    pub fn approve_step(step_id: &str, by: &str, note: &str) -> Event {
        Event {
            body: Some(event::Body::Approval(Approval {
                target: Some(approval::Target::StepId(step_id.into())),
                by: by.into(),
                at_ms: 0,
                note: note.into(),
            })),
        }
    }

    pub fn approve_scope(by: &str, note: &str) -> Event {
        Event {
            body: Some(event::Body::Approval(Approval {
                target: Some(approval::Target::Scope(Default::default())),
                by: by.into(),
                at_ms: 0,
                note: note.into(),
            })),
        }
    }

    pub fn budget_use(step_id: &str, by: &str, used: crate::Use) -> Event {
        Event {
            body: Some(event::Body::BudgetUse(BudgetUse {
                step_id: step_id.into(),
                builds: used.builds as u32,
                test_ms: used.test_ms,
                harnesses: used.harnesses as u32,
                games: used.games as u32,
                at_ms: 0,
                by: by.into(),
            })),
        }
    }
}

/// Running totals of a step's recorded `BudgetUse` (test time in milliseconds).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Use {
    pub builds: u64,
    pub test_ms: u64,
    pub harnesses: u64,
    pub games: u64,
}

impl Use {
    pub fn of(&self, kind: RunKind) -> u64 {
        match kind {
            RunKind::Build => self.builds,
            RunKind::Test => self.test_ms,
            RunKind::Harness => self.harnesses,
            RunKind::Game => self.games,
        }
    }
}

/// A step's current state: the fold of its events under the plan's rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepState {
    pub status: Status,
    /// True when the step may leave PLANNED: it needs no approval, or one was recorded.
    pub allowed: bool,
    pub landed: Option<Landed>,
    pub used: Use,
    pub by: String,
    pub detail: String,
    pub at_ms: u64,
}

impl StepState {
    pub fn done(&self) -> bool {
        self.status == Status::Landed
    }

    /// Waiting on the owner: gated, unapproved and not started or dropped.
    pub fn waiting_approval(&self) -> bool {
        !self.allowed && matches!(self.status, Status::Planned | Status::Held | Status::Failed)
    }
}

#[derive(Clone)]
struct Raw {
    status: Status,
    approved: bool,
    landed: Option<Landed>,
    used: Use,
    by: String,
    detail: String,
    at_ms: u64,
}

impl Default for Raw {
    fn default() -> Self {
        Self {
            status: Status::Planned,
            approved: false,
            landed: None,
            used: Use::default(),
            by: String::new(),
            detail: String::new(),
            at_ms: 0,
        }
    }
}

/// The fold of every event in the ledger file at one moment.
pub struct Snapshot {
    raw: BTreeMap<String, Raw>,
    scope_approved: bool,
    pub warnings: Vec<String>,
}

impl Snapshot {
    fn fold(events: &[Event]) -> Self {
        let mut s = Self {
            raw: BTreeMap::new(),
            scope_approved: false,
            warnings: Vec::new(),
        };
        for e in events {
            let Some(body) = &e.body else { continue };
            match body {
                event::Body::Status(st) => {
                    let status = st.status();
                    if status == Status::Unspecified {
                        continue;
                    }
                    let r = s.raw.entry(st.step_id.clone()).or_default();
                    r.status = status;
                    r.by = st.by.clone();
                    r.detail = st.detail.clone();
                    r.at_ms = st.at_ms;
                }
                event::Body::Landed(l) => {
                    let r = s.raw.entry(l.step_id.clone()).or_default();
                    r.status = Status::Landed;
                    r.landed = Some(l.clone());
                    r.at_ms = l.at_ms;
                }
                event::Body::Approval(a) => match &a.target {
                    Some(approval::Target::StepId(id)) => {
                        let r = s.raw.entry(id.clone()).or_default();
                        r.approved = true;
                        if r.status == Status::Planned {
                            r.status = Status::Approved;
                            r.by = a.by.clone();
                            r.detail = a.note.clone();
                            r.at_ms = a.at_ms;
                        }
                    }
                    Some(approval::Target::Scope(_)) => s.scope_approved = true,
                    None => {}
                },
                event::Body::BudgetUse(b) => {
                    let u = &mut s.raw.entry(b.step_id.clone()).or_default().used;
                    u.builds += u64::from(b.builds);
                    u.test_ms += b.test_ms;
                    u.harnesses += u64::from(b.harnesses);
                    u.games += u64::from(b.games);
                }
            }
        }
        s
    }

    pub fn scope_approved(&self) -> bool {
        self.scope_approved
    }

    pub fn state(&self, step: &Step) -> StepState {
        let r = self.raw.get(&step.id).cloned().unwrap_or_default();
        let allowed = !step.needs_approval()
            || r.approved
            || (self.scope_approved && step.covered_by_scope());
        StepState {
            status: r.status,
            allowed,
            landed: r.landed,
            used: r.used,
            by: r.by,
            detail: r.detail,
            at_ms: r.at_ms,
        }
    }

    /// Ids the ledger mentions.
    pub fn step_ids(&self) -> BTreeSet<&str> {
        self.raw.keys().map(String::as_str).collect()
    }
}

/// Why a tool should not start more work for a step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    NeedsApproval,
    Exhausted { used: u64, limit: u64 },
}

pub struct Ledger {
    root: PathBuf,
    path: PathBuf,
    plan: Plan,
}

impl Ledger {
    /// `root` holds `crew.toml`; the plan and ledger paths come from its `[plan]` table.
    pub fn open(root: &Path) -> Result<Self, Error> {
        let (plan, path) = plan::load(root)?;
        Ok(Self {
            root: root.to_path_buf(),
            path,
            plan,
        })
    }

    /// For callers that build the plan themselves.
    pub fn with_plan(plan: Plan, ledger_path: PathBuf) -> Self {
        Self {
            root: ledger_path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_default(),
            path: ledger_path,
            plan,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn plan(&self) -> &Plan {
        &self.plan
    }

    /// Reads and folds the whole file (shared lock, so a concurrent append is never seen half-written).
    pub fn snapshot(&self) -> Result<Snapshot, Error> {
        let mut file = match File::open(&self.path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Snapshot::fold(&[]));
            }
            Err(e) => return Err(Error::io(&self.path, e)),
        };
        file.lock_shared().map_err(|e| Error::io(&self.path, e))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|e| Error::io(&self.path, e))?;
        let parsed = store::parse(&self.path, &bytes)?;
        let mut snap = Snapshot::fold(&parsed.events);
        snap.warnings.extend(parsed.truncated_tail);
        Ok(snap)
    }

    pub fn state(&self, step_id: &str) -> Result<StepState, Error> {
        let step = self
            .plan
            .step(step_id)
            .ok_or_else(|| Error::UnknownStep(step_id.into()))?;
        Ok(self.snapshot()?.state(step))
    }

    /// Steps that are PLANNED/APPROVED, allowed to start and have every dep landed, in plan order.
    pub fn ready_steps(&self) -> Result<Vec<&Step>, Error> {
        let snap = self.snapshot()?;
        let landed = |id: &str| {
            self.plan
                .step(id)
                .is_some_and(|s| snap.state(s).status == Status::Landed)
        };
        Ok(self
            .plan
            .steps
            .iter()
            .filter(|s| {
                let st = snap.state(s);
                matches!(st.status, Status::Planned | Status::Approved)
                    && st.allowed
                    && s.deps.iter().all(|d| landed(d))
            })
            .collect())
    }

    /// Appends one event after checking it against the plan and its approval rules.
    pub fn record(&self, mut event: Event) -> Result<(), Error> {
        let now = now_ms();
        let stamp = |at: &mut u64| {
            if *at == 0 {
                *at = now;
            }
        };
        let Some(body) = event.body.as_mut() else {
            return Err(Error::Invalid("event has no body".into()));
        };
        let (step_id, starts_work) = match body {
            event::Body::Status(s) => {
                stamp(&mut s.at_ms);
                let status = s.status();
                if status == Status::Unspecified {
                    return Err(Error::Invalid("status is unspecified".into()));
                }
                let past_planned = matches!(
                    status,
                    Status::Approved | Status::Running | Status::Built | Status::Landed
                );
                (s.step_id.clone(), past_planned)
            }
            event::Body::Landed(l) => {
                stamp(&mut l.at_ms);
                if l.sha.is_empty() {
                    return Err(Error::Invalid("landed needs a sha".into()));
                }
                (l.step_id.clone(), true)
            }
            event::Body::Approval(a) => {
                stamp(&mut a.at_ms);
                match &a.target {
                    None => return Err(Error::Invalid("approval has no target".into())),
                    Some(approval::Target::Scope(_)) => (String::new(), false),
                    Some(approval::Target::StepId(id)) => (id.clone(), false),
                }
            }
            event::Body::BudgetUse(b) => {
                stamp(&mut b.at_ms);
                (b.step_id.clone(), false)
            }
        };
        let is_scope_approval = matches!(
            &event.body,
            Some(event::Body::Approval(Approval {
                target: Some(approval::Target::Scope(_)),
                ..
            }))
        );
        let step = if is_scope_approval {
            None
        } else {
            Some(
                self.plan
                    .step(&step_id)
                    .ok_or_else(|| Error::UnknownStep(step_id.clone()))?,
            )
        };

        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir).map_err(|e| Error::io(dir, e))?;
        }
        let io = |e| Error::io(&self.path, e);
        let mut file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(&self.path)
            .map_err(io)?;
        file.lock().map_err(io)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).map_err(io)?;
        let parsed = store::parse(&self.path, &bytes)?;
        if let (true, Some(step)) = (starts_work, step)
            && !Snapshot::fold(&parsed.events).state(step).allowed
        {
            return Err(Error::NeedsApproval(step.id.clone()));
        }
        if parsed.valid_len < bytes.len() {
            file.set_len(parsed.valid_len as u64).map_err(io)?;
        }
        file.write_all(&store::encode(&event)).map_err(io)?;
        file.sync_data().map_err(io)
    }

    /// `None` means the step may run another job of this kind; a spent budget refuses the next one.
    pub fn may_run(&self, step_id: &str, kind: RunKind) -> Result<Option<Refusal>, Error> {
        let step = self
            .plan
            .step(step_id)
            .ok_or_else(|| Error::UnknownStep(step_id.into()))?;
        let st = self.snapshot()?.state(step);
        if !st.allowed {
            return Ok(Some(Refusal::NeedsApproval));
        }
        let used = st.used.of(kind);
        Ok(step
            .limit(kind)
            .filter(|&limit| used >= limit)
            .map(|limit| Refusal::Exhausted { used, limit }))
    }
}
