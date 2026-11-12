//! `crew-plan`: the plan and run ledger CLI (`crew plan <sub>` execs this).

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use crew_ledger::{
    Error, Ledger, Refusal, RunKind, Use, events, parse_status, render_markdown, render_show, show,
    status_name,
};

const HELP: &str = "usage: crew plan [--root DIR] <command>
  check                                   rules; exit 1 on violation
  show [--json]                           counts per kind, then the steps
  report                                  one-screen summary for the owner
  render                                  markdown for PLAN.md
  approve <id|scope> [--note TEXT]
  status <id> <status> [--detail TEXT]    planned approved running built landed failed held dropped
  landed <id> <sha> [--batch ID]
  use <id> [--builds N] [--test-ms N] [--harnesses N] [--games N]
  next [--n N] [--json]                   steps ready to start, in plan order
  may-run <id> <build|test|harness|game>  exit 1 when the step must not start another run
The project root holds crew.toml (--root, CREW_ROOT, else the nearest parent of the cwd).";

type Res<T> = Result<T, String>;

fn flag(args: &mut Vec<String>, name: &str) -> Res<Option<String>> {
    let Some(i) = args.iter().position(|a| a == name) else {
        return Ok(None);
    };
    if i + 1 >= args.len() {
        return Err(format!("{name} needs a value"));
    }
    let v = args.remove(i + 1);
    args.remove(i);
    Ok(Some(v))
}

fn switch(args: &mut Vec<String>, name: &str) -> bool {
    let before = args.len();
    args.retain(|a| a != name);
    args.len() != before
}

fn number<T: std::str::FromStr>(args: &mut Vec<String>, name: &str) -> Res<Option<T>> {
    flag(args, name)?
        .map(|v| v.parse().map_err(|_| format!("bad {name} `{v}`")))
        .transpose()
}

fn positionals(args: Vec<String>, want: usize, usage: &str) -> Res<Vec<String>> {
    if let Some(bad) = args.iter().find(|a| a.starts_with("--")) {
        return Err(format!("unknown option `{bad}`"));
    }
    if args.len() != want {
        return Err(format!("usage: crew plan {usage}"));
    }
    Ok(args)
}

fn find_root(explicit: Option<String>) -> Res<PathBuf> {
    if let Some(r) = explicit.or_else(|| std::env::var("CREW_ROOT").ok()) {
        return Ok(PathBuf::from(r));
    }
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    cwd.ancestors()
        .find(|d| d.join("crew.toml").is_file())
        .map(Path::to_path_buf)
        .ok_or_else(|| "no crew.toml here or in a parent (use --root)".to_string())
}

fn by() -> String {
    std::env::var("CREW_AGENT")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "unknown".into())
}

fn json<T: serde::Serialize>(v: &T) -> Res<String> {
    serde_json::to_string_pretty(v).map_err(|e| e.to_string())
}

fn e(err: Error) -> String {
    err.to_string()
}

fn warn(ledger: &Ledger) {
    if let Ok(snap) = ledger.snapshot() {
        for w in &snap.warnings {
            eprintln!("warning: {w}");
        }
    }
}

fn run(mut args: Vec<String>) -> Res<ExitCode> {
    let root_flag = flag(&mut args, "--root")?;
    if args.is_empty() || args[0] == "--help" || args[0] == "-h" || args[0] == "help" {
        println!("{HELP}");
        return Ok(ExitCode::SUCCESS);
    }
    let sub = args.remove(0);
    let ledger = Ledger::open(&find_root(root_flag)?).map_err(e)?;
    warn(&ledger);
    match sub.as_str() {
        "check" => {
            positionals(args, 0, "check")?;
            let violations = crew_ledger::check(&ledger).map_err(e)?;
            for v in &violations {
                println!("violation: {v}");
            }
            if violations.is_empty() {
                println!("ok: {} steps", ledger.plan().steps.len());
                return Ok(ExitCode::SUCCESS);
            }
            return Ok(ExitCode::from(1));
        }
        "show" => {
            let as_json = switch(&mut args, "--json");
            positionals(args, 0, "show [--json]")?;
            let s = show(&ledger).map_err(e)?;
            if as_json {
                println!("{}", json(&s)?);
            } else {
                print!("{}", render_show(&ledger, &s));
            }
        }
        "report" => {
            positionals(args, 0, "report")?;
            let s = show(&ledger).map_err(e)?;
            print!("{}", s.summary.render_report(&s));
        }
        "render" => {
            positionals(args, 0, "render")?;
            let s = show(&ledger).map_err(e)?;
            print!("{}", render_markdown(&ledger, &s));
        }
        "approve" => {
            let note = flag(&mut args, "--note")?.unwrap_or_default();
            let target = positionals(args, 1, "approve <id|scope> [--note TEXT]")?.remove(0);
            let ev = if target == "scope" {
                events::approve_scope(&by(), &note)
            } else {
                events::approve_step(&target, &by(), &note)
            };
            ledger.record(ev).map_err(e)?;
            println!("approved {target}");
        }
        "status" => {
            let detail = flag(&mut args, "--detail")?.unwrap_or_default();
            let a = positionals(args, 2, "status <id> <status> [--detail TEXT]")?;
            let status = parse_status(&a[1]).ok_or_else(|| format!("unknown status `{}`", a[1]))?;
            ledger
                .record(events::status(&a[0], status, &by(), &detail))
                .map_err(e)?;
            println!("{} is {}", a[0], status_name(status));
        }
        "landed" => {
            let batch = flag(&mut args, "--batch")?.unwrap_or_default();
            let a = positionals(args, 2, "landed <id> <sha> [--batch ID]")?;
            ledger
                .record(events::landed(&a[0], &a[1], &batch))
                .map_err(e)?;
            println!("{} landed at {}", a[0], a[1]);
        }
        "use" => {
            let used = Use {
                builds: number(&mut args, "--builds")?.unwrap_or(0),
                test_ms: number(&mut args, "--test-ms")?.unwrap_or(0),
                harnesses: number(&mut args, "--harnesses")?.unwrap_or(0),
                games: number(&mut args, "--games")?.unwrap_or(0),
            };
            if used == Use::default() {
                return Err(
                    "nothing to record: give --builds, --test-ms, --harnesses or --games".into(),
                );
            }
            let id = positionals(args, 1, "use <id> [--builds N] [--test-ms N] ...")?.remove(0);
            ledger
                .record(events::budget_use(&id, &by(), used))
                .map_err(e)?;
            if let Some((_, kinds)) =
                crew_ledger::over_budget(ledger.plan(), &ledger.snapshot().map_err(e)?)
                    .into_iter()
                    .find(|(s, _)| *s == id)
            {
                let names: Vec<&str> = kinds.iter().map(|k| k.name()).collect();
                println!("recorded; {id} is now over budget on {}", names.join(", "));
            } else {
                println!("recorded");
            }
        }
        "next" => {
            let as_json = switch(&mut args, "--json");
            let n: Option<usize> = number(&mut args, "--n")?;
            positionals(args, 0, "next [--n N] [--json]")?;
            let ready = ledger.ready_steps().map_err(e)?;
            let ready: Vec<_> = ready.into_iter().take(n.unwrap_or(usize::MAX)).collect();
            if as_json {
                println!("{}", json(&ready)?);
            } else {
                for s in ready {
                    println!("{}\t{}\t{}", s.id, s.kind.name(), s.title);
                }
            }
        }
        "may-run" => {
            let a = positionals(args, 2, "may-run <id> <build|test|harness|game>")?;
            let kind =
                RunKind::from_name(&a[1]).ok_or_else(|| format!("unknown run kind `{}`", a[1]))?;
            match ledger.may_run(&a[0], kind).map_err(e)? {
                None => println!("ok"),
                Some(Refusal::NeedsApproval) => {
                    eprintln!("refused: {} needs an owner approval first", a[0]);
                    return Ok(ExitCode::from(1));
                }
                Some(Refusal::Exhausted { used, limit }) => {
                    let (used, limit) = if kind == RunKind::Test {
                        (used / 60_000, limit / 60_000)
                    } else {
                        (used, limit)
                    };
                    let unit = if kind == RunKind::Test { " min" } else { "" };
                    eprintln!(
                        "refused: {} has used {used}{unit} of its {limit}{unit} {} budget; the owner can raise it in plan.toml, or run it in the final pass",
                        a[0],
                        kind.name()
                    );
                    return Ok(ExitCode::from(1));
                }
            }
        }
        other => return Err(format!("unknown command `{other}`\n{HELP}")),
    }
    Ok(ExitCode::SUCCESS)
}

fn main() -> ExitCode {
    match run(std::env::args().skip(1).collect()) {
        Ok(code) => code,
        Err(msg) => {
            eprintln!("crew-plan: {msg}");
            ExitCode::from(2)
        }
    }
}
