//! What `crew cargo` runs: the program, its arguments and the environment, assembled from settings, the grant and the caller's own environment.
use std::collections::BTreeMap;
use std::path::Path;

use crew_depot_config::Depot;
use crew_proto::{BuildGrant, BuildPriority};

pub struct Inputs<'a> {
    pub depot: &'a Depot,
    /// Everything after `crew cargo`, as the user typed it.
    pub args: &'a [String],
    /// The caller's environment: a variable set there is never overridden.
    pub env: &'a BTreeMap<String, String>,
    pub mbx: Option<&'a Path>,
    pub grant: Option<&'a BuildGrant>,
    /// Where mbx writes its build report.
    pub report: Option<&'a Path>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub argv: Vec<String>,
    pub env_set: BTreeMap<String, String>,
    pub env_unset: Vec<String>,
    /// One-line remarks for the user, printed before the build starts.
    pub notes: Vec<String>,
}

/// Arguments before a lone `--` that already pick a job count.
fn names_jobs(args: &[String]) -> bool {
    args.iter()
        .take_while(|a| a.as_str() != "--")
        .any(|a| a == "--jobs" || a.starts_with("--jobs=") || a == "-j" || a.starts_with("-j"))
}

pub fn assemble(i: &Inputs<'_>) -> Plan {
    let mut plan = Plan::default();
    let mut set = |key: &str, value: String| {
        if !i.env.contains_key(key) {
            plan.env_set.insert(key.to_string(), value);
        }
    };
    let has_plus_toolchain = i.args.first().is_some_and(|a| a.starts_with('+'));
    if let Some(t) = &i.depot.toolchain
        && !has_plus_toolchain
    {
        set("RUSTUP_TOOLCHAIN", t.clone());
    }
    for (k, v) in i.depot.profile_env() {
        set(&k, v);
    }
    if let Some(b) = i.grant {
        if b.compile_cpus > 0 && !names_jobs(i.args) {
            set("CARGO_BUILD_JOBS", b.compile_cpus.to_string());
        }
        if i.mbx.is_some() && b.priority == BuildPriority::Low as i32 {
            set("MBX_SCHEDULER_PRIORITY", "low".into());
        }
    }
    match i.mbx {
        Some(mbx) => {
            plan.argv.push(mbx.display().to_string());
            if let Some(r) = i.report {
                set("MBX_STATS_REPORT", r.display().to_string());
            }
            // mbx defers to another wrapper and then caches nothing, so a leftover one (sccache) would silently defeat it.
            if let Some(w) = i.env.get("RUSTC_WRAPPER").filter(|w| !w.is_empty()) {
                plan.env_unset.push("RUSTC_WRAPPER".into());
                plan.notes.push(format!(
                    "crew: ignoring RUSTC_WRAPPER={w}; mbx cannot share it"
                ));
            }
        }
        None => plan.argv.push("cargo".into()),
    }
    plan.argv.extend(i.args.iter().cloned());
    plan
}

#[cfg(test)]
mod tests;
