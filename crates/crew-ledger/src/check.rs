use std::collections::{BTreeSet, HashMap};
use std::fmt;

use crate::glob;
use crate::ledger::{Ledger, Snapshot};
use crate::plan::{Plan, RESERVED_SCOPE_ID, RunKind};
use crate::{Error, Status};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Violation {
    NoRequest,
    EmptyId,
    ReservedId(String),
    DuplicateId(String),
    UnknownDep {
        step: String,
        dep: String,
    },
    Cycle(Vec<String>),
    Overlap {
        a: String,
        b: String,
        glob_a: String,
        glob_b: String,
    },
    /// A gated step is past PLANNED with no approval on record.
    Unapproved {
        step: String,
        status: Status,
    },
    /// The ledger mentions a step the plan does not have.
    UnknownStepInLedger(String),
    OverBudget {
        step: String,
        kinds: Vec<RunKind>,
    },
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoRequest => write!(
                f,
                "the plan does not state the request it serves ([plan] request)"
            ),
            Self::EmptyId => write!(f, "a step has an empty id"),
            Self::ReservedId(id) => write!(f, "step id `{id}` is reserved"),
            Self::DuplicateId(id) => write!(f, "step id `{id}` is used twice"),
            Self::UnknownDep { step, dep } => {
                write!(f, "step `{step}` depends on unknown step `{dep}`")
            }
            Self::Cycle(ids) => write!(f, "dependency cycle: {}", ids.join(" -> ")),
            Self::Overlap {
                a,
                b,
                glob_a,
                glob_b,
            } => write!(
                f,
                "steps `{a}` and `{b}` have no dependency path but their owns overlap (`{glob_a}` / `{glob_b}`)"
            ),
            Self::Unapproved { step, status } => write!(
                f,
                "step `{step}` is {} without an approval",
                crate::status_name(*status)
            ),
            Self::UnknownStepInLedger(id) => {
                write!(f, "the ledger has events for unknown step `{id}`")
            }
            Self::OverBudget { step, kinds } => write!(
                f,
                "step `{step}` is over budget on {}",
                kinds
                    .iter()
                    .map(|k| k.name())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}

/// Plan-only rules: ids, deps, cycles, overlapping ownership.
pub fn check_plan(plan: &Plan) -> Vec<Violation> {
    let mut out = Vec::new();
    if plan.request.is_empty() {
        out.push(Violation::NoRequest);
    }
    let mut index: HashMap<&str, usize> = HashMap::new();
    let mut seen_dup = BTreeSet::new();
    for (i, s) in plan.steps.iter().enumerate() {
        if s.id.is_empty() {
            out.push(Violation::EmptyId);
        } else if s.id == RESERVED_SCOPE_ID {
            out.push(Violation::ReservedId(s.id.clone()));
        }
        if index.insert(&s.id, i).is_some() && seen_dup.insert(s.id.as_str()) {
            out.push(Violation::DuplicateId(s.id.clone()));
        }
    }
    let n = plan.steps.len();
    let mut deps: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, s) in plan.steps.iter().enumerate() {
        for d in &s.deps {
            match index.get(d.as_str()) {
                Some(&j) => deps[i].push(j),
                None => out.push(Violation::UnknownDep {
                    step: s.id.clone(),
                    dep: d.clone(),
                }),
            }
        }
    }
    out.extend(cycles(plan, &deps).into_iter().map(Violation::Cycle));

    let reach: Vec<BTreeSet<usize>> = (0..n)
        .map(|i| {
            let mut seen = BTreeSet::new();
            let mut stack = deps[i].clone();
            while let Some(j) = stack.pop() {
                if seen.insert(j) {
                    stack.extend(&deps[j]);
                }
            }
            seen
        })
        .collect();
    for i in 0..n {
        for j in i + 1..n {
            if reach[i].contains(&j) || reach[j].contains(&i) {
                continue;
            }
            let (a, b) = (&plan.steps[i], &plan.steps[j]);
            let hit = a.owns.iter().find_map(|ga| {
                b.owns
                    .iter()
                    .find(|gb| glob::overlaps(ga, gb))
                    .map(|gb| (ga, gb))
            });
            if let Some((ga, gb)) = hit {
                out.push(Violation::Overlap {
                    a: a.id.clone(),
                    b: b.id.clone(),
                    glob_a: ga.clone(),
                    glob_b: gb.clone(),
                });
            }
        }
    }
    out
}

fn cycles(plan: &Plan, deps: &[Vec<usize>]) -> Vec<Vec<String>> {
    #[derive(Clone, Copy, PartialEq)]
    enum Color {
        White,
        Gray,
        Black,
    }
    fn visit(
        i: usize,
        deps: &[Vec<usize>],
        color: &mut [Color],
        stack: &mut Vec<usize>,
        found: &mut Vec<Vec<usize>>,
    ) {
        color[i] = Color::Gray;
        stack.push(i);
        for &j in &deps[i] {
            match color[j] {
                Color::White => visit(j, deps, color, stack, found),
                Color::Gray => {
                    let at = stack.iter().position(|&x| x == j).unwrap_or(0);
                    let mut cyc = stack[at..].to_vec();
                    cyc.push(j);
                    found.push(cyc);
                }
                Color::Black => {}
            }
        }
        stack.pop();
        color[i] = Color::Black;
    }
    let mut color = vec![Color::White; deps.len()];
    let mut found = Vec::new();
    for i in 0..deps.len() {
        if color[i] == Color::White {
            visit(i, deps, &mut color, &mut Vec::new(), &mut found);
        }
    }
    let mut seen = BTreeSet::new();
    found
        .into_iter()
        .filter(|c| seen.insert(c.iter().copied().collect::<BTreeSet<_>>()))
        .map(|c| c.into_iter().map(|i| plan.steps[i].id.clone()).collect())
        .collect()
}

/// Steps and kinds whose recorded use exceeds the budget.
pub fn over_budget(plan: &Plan, snap: &Snapshot) -> Vec<(String, Vec<RunKind>)> {
    plan.steps
        .iter()
        .filter_map(|s| {
            let st = snap.state(s);
            let kinds: Vec<RunKind> = RunKind::ALL
                .into_iter()
                .filter(|&k| s.limit(k).is_some_and(|l| st.used.of(k) > l))
                .collect();
            (!kinds.is_empty()).then(|| (s.id.clone(), kinds))
        })
        .collect()
}

/// Every rule: the plan's own, then the ledger against the plan.
pub fn check(ledger: &Ledger) -> Result<Vec<Violation>, Error> {
    let plan = ledger.plan();
    let mut out = check_plan(plan);
    let snap = ledger.snapshot()?;
    for s in &plan.steps {
        let st = snap.state(s);
        let started = matches!(st.status, Status::Running | Status::Built | Status::Landed);
        if started && !st.allowed {
            out.push(Violation::Unapproved {
                step: s.id.clone(),
                status: st.status,
            });
        }
    }
    for id in snap.step_ids() {
        if plan.step(id).is_none() {
            out.push(Violation::UnknownStepInLedger(id.to_string()));
        }
    }
    out.extend(
        over_budget(plan, &snap)
            .into_iter()
            .map(|(step, kinds)| Violation::OverBudget { step, kinds }),
    );
    Ok(out)
}
