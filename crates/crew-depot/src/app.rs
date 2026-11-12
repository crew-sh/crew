//! The commands behind `crew cargo` and `crew build`.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use crew_depot_config::{Depot, DepotTable, MbxMode};
use crew_proto::RefusalReason;
use crew_usher_client::{Client, ClientError, Launch, RunOptions};
use serde::Serialize;

use crate::cli::ReportArgs;
use crate::plan::{self, Inputs};
use crate::report::{self, GraphRun};
use crate::stats::{self, Build, MbxReport};
use crate::{mbx, why};

/// A report older than this belongs to a run that died; no build takes a day.
const REPORT_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(24 * 3600);

pub const EXIT_ERROR: u8 = 125;
pub const EXIT_REFUSED: u8 = 75;

/// One line per user-only key the project's crew.toml sets; they are ignored.
pub fn ignored_project_keys(dir: &Path) -> Result<Vec<String>> {
    Ok(DepotTable::project(dir)?
        .user_only_keys_set()
        .into_iter()
        .map(|key| {
            format!(
                "crew: {key} in this project's crew.toml is ignored: a checked-out branch must not choose what runs or where records go; set it in depot.toml"
            )
        })
        .collect())
}

/// User settings, then the project's on top.
pub fn depot_for(dir: &Path) -> Result<Depot> {
    Ok(DepotTable::user()?
        .layered(DepotTable::project(dir)?)
        .resolve())
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn repo_of(cwd: &Path) -> PathBuf {
    crew_sys::paths::repo_root(cwd).unwrap_or_else(|| cwd.to_path_buf())
}

fn report_file(store: &Path) -> Option<PathBuf> {
    let dir = store.join("stats");
    std::fs::create_dir_all(&dir).ok()?;
    stats::sweep_reports(&dir, REPORT_MAX_AGE);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    Some(dir.join(format!("run-{}-{nanos}.json", std::process::id())))
}

fn fail(e: &ClientError) -> ExitCode {
    eprintln!("crew: {e}");
    let refused =
        matches!(e, ClientError::Refused { reason, .. } if *reason != RefusalReason::Unspecified);
    ExitCode::from(if refused { EXIT_REFUSED } else { EXIT_ERROR })
}

/// Folds mbx's build report into the repository's record, then removes the report.
fn record_build(store: &Path, root: &Path, args: &[String], report: &Path) {
    if let Ok(text) = std::fs::read_to_string(report)
        && let Ok(r) = serde_json::from_str::<MbxReport>(&text)
    {
        let build = Build {
            root,
            command: &args.join(" "),
            counts: r.counts(),
            at_secs: now_secs(),
        };
        if let Err(e) = stats::record(store, &build) {
            eprintln!("crew: could not record build statistics: {e}");
        }
    }
    let _ = std::fs::remove_file(report);
}

pub fn cargo(args: &[String]) -> Result<ExitCode> {
    let cwd = std::env::current_dir()?;
    let depot = depot_for(&cwd)?;
    for note in ignored_project_keys(&cwd)? {
        eprintln!("{note}");
    }
    for note in plan::usher_side_overrides(&DepotTable::user()?.resolve(), &depot) {
        eprintln!("{note}");
    }
    let mbx = depot.find_mbx();
    match (depot.mbx, &mbx) {
        (MbxMode::Required, None) => bail!("depot.mbx is \"required\" but mbx was not found"),
        (MbxMode::Auto, None) => eprintln!("crew: mbx not found; building with plain cargo"),
        _ => {}
    }
    let env: BTreeMap<String, String> = std::env::vars().collect();
    let report = mbx.as_ref().and_then(|_| report_file(&depot.store));
    let mut argv = vec!["cargo".to_string()];
    argv.extend_from_slice(args);
    let opts = RunOptions {
        argv,
        strict: env.get("CREW_STRICT").is_some_and(|v| v == "1"),
        claimed_job: env.get("CREW_JOB").cloned(),
        ..RunOptions::default()
    };
    let result = Client::from_env().run_with(&opts, |grant| {
        let plan = plan::assemble(&Inputs {
            depot: &depot,
            args,
            env: &env,
            mbx: mbx.as_deref(),
            grant: grant.and_then(|g| g.build.as_ref()),
            report: report.as_deref(),
        });
        for note in &plan.notes {
            eprintln!("{note}");
        }
        Launch {
            argv: Some(plan.argv),
            env_set: plan.env_set,
            env_unset: plan.env_unset,
        }
    });
    if let Some(r) = &report {
        record_build(&depot.store, &repo_of(&cwd), args, r);
    }
    Ok(match result {
        Ok(code) => ExitCode::from(crew_usher_client::jobs::exit_byte(code)),
        Err(e) => fail(&e),
    })
}

fn print_json<T: Serialize>(v: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

pub fn build_stats(json: bool) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let depot = depot_for(&cwd)?;
    let s = stats::load(&stats::record_path(&depot.store, &repo_of(&cwd)));
    let life = depot.find_mbx().and_then(|m| mbx::lifetime(&m));
    if json {
        return print_json(&stats::json(s.as_ref(), life));
    }
    print!("{}", stats::render(s.as_ref(), life.as_ref(), now_secs()));
    Ok(())
}

#[derive(Serialize)]
struct WhyJson {
    krate: String,
    missed: Option<why::Miss>,
    recorded_misses: Vec<String>,
}

pub fn build_why(krate: &str, json: bool) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let depot = depot_for(&cwd)?;
    let mbx = depot
        .find_mbx()
        .context("mbx is not installed, so there is no build record to read")?;
    let explained = why::parse(&mbx::explain_last(&mbx, &cwd)?);
    if json {
        return print_json(&WhyJson {
            krate: krate.to_string(),
            missed: why::find(&explained, krate).cloned(),
            recorded_misses: explained.missed.iter().map(|m| m.krate.clone()).collect(),
        });
    }
    print!("{}", why::render(&explained, krate));
    Ok(())
}

struct Side {
    graph: report::UnitGraph,
    rustc: String,
}

fn side(dir: &Path, args: &ReportArgs) -> Result<Side> {
    let depot = depot_for(dir)?;
    let env = depot.profile_env();
    let run = GraphRun {
        dir,
        check: args.check,
        profile: args.profile.as_deref(),
        toolchain: depot.toolchain.as_deref(),
        env: &env,
    };
    Ok(Side {
        rustc: report::rustc_version(&run)?,
        graph: report::unit_graph(&run)?,
    })
}

pub fn build_report(args: &ReportArgs) -> Result<()> {
    let here_dir = std::env::current_dir()?;
    let there_dir = args
        .shared
        .canonicalize()
        .with_context(|| format!("{} is not a directory", args.shared.display()))?;
    let (here, there) = (side(&here_dir, args)?, side(&there_dir, args)?);
    let r = report::compare(&here.graph, &there.graph, &here.rustc, &there.rustc);
    if args.json {
        return print_json(&r);
    }
    print!("{}", report::render(&r, args.summary));
    Ok(())
}
