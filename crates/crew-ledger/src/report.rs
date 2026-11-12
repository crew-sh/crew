use std::fmt::Write as _;

use serde::Serialize;

use crate::check::over_budget;
use crate::ledger::{Ledger, StepState, Use};
use crate::plan::{Kind, RunKind, Step};
use crate::{Error, Status, status_name};

#[derive(Debug, Clone, Serialize)]
pub struct KindCount {
    pub kind: Kind,
    pub done: usize,
    /// Every step of the kind except dropped ones.
    pub total: usize,
    pub dropped: usize,
    pub waiting_approval: usize,
}

impl KindCount {
    fn label(&self) -> &'static str {
        match self.kind {
            Kind::CoreDesign => "core",
            k => k.name(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Failure {
    pub step: String,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct OverBudget {
    pub step: String,
    pub kinds: Vec<RunKind>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Summary {
    pub request: String,
    pub kinds: Vec<KindCount>,
    pub failed: Vec<Failure>,
    pub held: Vec<String>,
    pub over_budget: Vec<OverBudget>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StepRow {
    pub id: String,
    pub title: String,
    pub kind: Kind,
    pub status: &'static str,
    pub allowed: bool,
    pub deps: Vec<String>,
    pub model: crate::Model,
    pub used: Use,
    pub landed_sha: Option<String>,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Show {
    pub summary: Summary,
    pub steps: Vec<StepRow>,
}

pub fn summarize(ledger: &Ledger) -> Result<Summary, Error> {
    Ok(show(ledger)?.summary)
}

pub fn show(ledger: &Ledger) -> Result<Show, Error> {
    let plan = ledger.plan();
    let snap = ledger.snapshot()?;
    let mut kinds: Vec<KindCount> = Kind::ALL
        .into_iter()
        .map(|kind| KindCount {
            kind,
            done: 0,
            total: 0,
            dropped: 0,
            waiting_approval: 0,
        })
        .collect();
    let mut failed = Vec::new();
    let mut held = Vec::new();
    let mut steps = Vec::new();
    for s in &plan.steps {
        let st = snap.state(s);
        let c = kinds
            .iter_mut()
            .find(|c| c.kind == s.kind)
            .expect("every kind listed");
        if st.status == Status::Dropped {
            c.dropped += 1;
        } else {
            c.total += 1;
            c.done += usize::from(st.done());
            c.waiting_approval += usize::from(st.waiting_approval());
        }
        match st.status {
            Status::Failed => failed.push(Failure {
                step: s.id.clone(),
                detail: st.detail.clone(),
            }),
            Status::Held => held.push(s.id.clone()),
            _ => {}
        }
        steps.push(row(s, st));
    }
    let over_budget = over_budget(plan, &snap)
        .into_iter()
        .map(|(step, kinds)| OverBudget { step, kinds })
        .collect();
    Ok(Show {
        summary: Summary {
            request: plan.request.clone(),
            kinds,
            failed,
            held,
            over_budget,
            warnings: snap.warnings,
        },
        steps,
    })
}

fn row(s: &Step, st: StepState) -> StepRow {
    StepRow {
        id: s.id.clone(),
        title: s.title.clone(),
        kind: s.kind,
        status: status_name(st.status),
        allowed: st.allowed,
        deps: s.deps.clone(),
        model: s.model,
        used: st.used,
        landed_sha: st.landed.map(|l| l.sha),
        detail: st.detail,
    }
}

impl Summary {
    /// "asked 41/44 · extra 31/163 (12 waiting approval) · core 3/50 (2 waiting approval)"
    pub fn counts_line(&self) -> String {
        let parts: Vec<String> = self
            .kinds
            .iter()
            .map(|c| {
                let mut p = format!("{} {}/{}", c.label(), c.done, c.total);
                if c.waiting_approval > 0 {
                    let _ = write!(p, " ({} waiting approval)", c.waiting_approval);
                }
                p
            })
            .collect();
        parts.join(" · ")
    }

    /// One screen for the owner: what was asked, what was added, what waits on them, what it costs.
    pub fn render_report(&self, show: &Show) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "request: {}",
            if self.request.is_empty() {
                "(none stated)"
            } else {
                &self.request
            }
        );
        let _ = writeln!(out, "{}", self.counts_line());
        for c in self.kinds.iter().filter(|c| c.dropped > 0) {
            let _ = writeln!(out, "dropped {}: {}", c.label(), c.dropped);
        }
        let waiting: Vec<&StepRow> = show
            .steps
            .iter()
            .filter(|r| !r.allowed && matches!(r.status, "planned" | "held" | "failed"))
            .collect();
        if !waiting.is_empty() {
            let _ = writeln!(out, "\nwaiting on you:");
            for r in waiting {
                let _ = writeln!(out, "  {} [{}] {}", r.id, r.kind.name(), r.title);
            }
        }
        if !self.failed.is_empty() {
            let _ = writeln!(out, "\nfailed:");
            for f in &self.failed {
                let first = f.detail.lines().next().unwrap_or("");
                let _ = writeln!(out, "  {}: {first}", f.step);
            }
        }
        if !self.held.is_empty() {
            let _ = writeln!(out, "\nheld: {}", self.held.join(", "));
        }
        if !self.over_budget.is_empty() {
            let _ = writeln!(out, "\nover budget:");
            for o in &self.over_budget {
                let kinds: Vec<&str> = o.kinds.iter().map(|k| k.name()).collect();
                let _ = writeln!(out, "  {}: {}", o.step, kinds.join(", "));
            }
        }
        for w in &self.warnings {
            let _ = writeln!(out, "warning: {w}");
        }
        out
    }
}

fn budget_cell(used: u64, limit: Option<u64>, unit: u64) -> String {
    match limit {
        Some(l) => format!("{}/{}", used / unit, l / unit),
        None => format!("{}", used / unit),
    }
}

/// The table `show` prints: kind counts, then one line per step.
pub fn render_show(ledger: &Ledger, show: &Show) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{}\n", show.summary.counts_line());
    let mut rows: Vec<[String; 6]> = vec![[
        "ID".into(),
        "KIND".into(),
        "STATUS".into(),
        "MODEL".into(),
        "USE b/t(min)/h/g".into(),
        "DEPS".into(),
    ]];
    for (r, s) in show.steps.iter().zip(&ledger.plan().steps) {
        let status = if !r.allowed && r.status == "planned" {
            "needs approval".to_string()
        } else {
            r.status.to_string()
        };
        let cells = [
            budget_cell(r.used.builds, s.limit(RunKind::Build), 1),
            budget_cell(r.used.test_ms, s.limit(RunKind::Test), 60_000),
            budget_cell(r.used.harnesses, s.limit(RunKind::Harness), 1),
            budget_cell(r.used.games, s.limit(RunKind::Game), 1),
        ];
        rows.push([
            r.id.clone(),
            r.kind.name().into(),
            status,
            format!("{:?}", r.model).to_lowercase(),
            cells.join(" "),
            r.deps.join(","),
        ]);
    }
    let widths: Vec<usize> = (0..6)
        .map(|i| rows.iter().map(|r| r[i].chars().count()).max().unwrap_or(0))
        .collect();
    for r in &rows {
        let line: Vec<String> = r
            .iter()
            .zip(&widths)
            .map(|(c, w)| format!("{c:<w$}"))
            .collect();
        let _ = writeln!(out, "{}", line.join("  ").trim_end());
    }
    out
}

/// Markdown for PLAN.md: one section per kind, done boxes, owner gates marked.
pub fn render_markdown(ledger: &Ledger, show: &Show) -> String {
    let mut out = String::from("# Plan\n\n");
    let _ = writeln!(out, "> {}\n", show.summary.request.replace('\n', " "));
    let _ = writeln!(out, "{}\n", show.summary.counts_line());
    for c in &show.summary.kinds {
        let heading = match c.kind {
            Kind::Asked => "Asked",
            Kind::Extra => "Extra (added; needs the owner's yes)",
            Kind::CoreDesign => "Core design (owner approves the design first)",
        };
        let _ = writeln!(out, "## {heading}: {}/{}\n", c.done, c.total);
        for (r, s) in show.steps.iter().zip(&ledger.plan().steps) {
            if r.kind != c.kind {
                continue;
            }
            let mark = if r.status == "landed" { "x" } else { " " };
            let tag = match c.kind {
                Kind::Asked => "",
                Kind::Extra => " **[EXTRA]**",
                Kind::CoreDesign => " **[CORE]**",
            };
            let mut note = vec![r.status.to_string()];
            if !r.allowed {
                note.push("awaiting owner approval".into());
            }
            if !r.deps.is_empty() {
                note.push(format!("after {}", r.deps.join(", ")));
            }
            if let Some(sha) = &r.landed_sha {
                note.push(format!(
                    "landed {}",
                    sha.chars().take(10).collect::<String>()
                ));
            }
            if let Some(n) = s.issue {
                note.push(format!("#{n}"));
            }
            let _ = writeln!(
                out,
                "- [{mark}] `{}`{tag} {} ({})",
                r.id,
                r.title,
                note.join("; ")
            );
        }
        out.push('\n');
    }
    out
}
