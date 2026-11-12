use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Result, bail};
use crew_queue::check::{CheckOutcome, CheckRunner};
use crew_queue::runner::Runner;
use crew_queue::{Ctx, status, submit, worktrees};

const USAGE: &str = "\
usage: crew queue [--repo DIR] <command>

  submit <branch> --step ID     record the step BUILT and queue its branch
  run [--once]                  land ready branches in batches (single runner per repo)
  status [--json]               queued, landed and failed steps; worktrees and their owners
  worktree new <step> [--owner-pid N]   create the step's worktree (a dead owner's is taken over)
  worktree done <step>          mark finished and drop its target dir
  salvage [<step>] [--all-dead] --to DIR   save diff, untracked files and unlanded commits
  gc                            salvage then remove worktrees whose owner is dead
  final                         run the [queue] final commands on the integration tip";

struct Args(Vec<String>);

impl Args {
    fn flag(&mut self, name: &str) -> bool {
        let before = self.0.len();
        self.0.retain(|a| a != name);
        self.0.len() != before
    }

    fn value(&mut self, name: &str) -> Result<Option<String>> {
        let Some(i) = self.0.iter().position(|a| a == name) else {
            return Ok(None);
        };
        if i + 1 >= self.0.len() {
            bail!("{name} needs a value");
        }
        let v = self.0.remove(i + 1);
        self.0.remove(i);
        Ok(Some(v))
    }

    fn positional(&mut self) -> Option<String> {
        (!self.0.is_empty()).then(|| self.0.remove(0))
    }

    fn finish(self) -> Result<()> {
        if let Some(extra) = self.0.first() {
            bail!("unexpected argument `{extra}`");
        }
        Ok(())
    }
}

fn run() -> Result<u8> {
    let mut args = Args(std::env::args().skip(1).collect());
    if args.flag("--help") || args.flag("-h") {
        println!("{USAGE}");
        return Ok(0);
    }
    let repo = args.value("--repo")?.map_or_else(
        || std::env::current_dir().map_err(Into::into),
        |p| Ok::<_, anyhow::Error>(PathBuf::from(p)),
    )?;
    let Some(cmd) = args.positional() else {
        bail!("no command\n\n{USAGE}")
    };
    let ctx = Ctx::discover(&repo)?;
    match cmd.as_str() {
        "submit" => {
            let step = args.value("--step")?;
            let branch = args.positional();
            args.finish()?;
            let (Some(step), Some(branch)) = (step, branch) else {
                bail!("usage: submit <branch> --step ID")
            };
            submit::submit(&ctx, &branch, &step)?;
            println!("queued {branch} for step {step}");
        }
        "run" => {
            let once = args.flag("--once");
            args.finish()?;
            let s = Runner::new(&ctx, CheckRunner::discover()).run(once)?;
            println!(
                "queue: {} landed, {} failed in {} batches",
                s.landed.len(),
                s.failed.len(),
                s.batches
            );
            return Ok(u8::from(!s.push_errors.is_empty()));
        }
        "status" => {
            let json = args.flag("--json");
            args.finish()?;
            let report = status::report(&ctx)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print!("{}", status::render(&report));
            }
        }
        "worktree" => {
            let sub = args.positional();
            match sub.as_deref() {
                Some("new") => {
                    let owner = args
                        .value("--owner-pid")?
                        .map(|v| v.parse::<u64>())
                        .transpose()?;
                    let step = args.positional();
                    args.finish()?;
                    let Some(step) = step else {
                        bail!("usage: worktree new <step>")
                    };
                    println!("{}", worktrees::new(&ctx, &step, owner)?.display());
                }
                Some("done") => {
                    let step = args.positional();
                    args.finish()?;
                    let Some(step) = step else {
                        bail!("usage: worktree done <step>")
                    };
                    worktrees::done(&ctx, &step)?;
                }
                _ => bail!("usage: worktree new|done <step>"),
            }
        }
        "salvage" => {
            let all_dead = args.flag("--all-dead");
            let Some(to) = args.value("--to")? else {
                bail!("salvage needs --to DIR")
            };
            let step = args.positional();
            args.finish()?;
            for row in worktrees::salvage(&ctx, step.as_deref(), all_dead, &PathBuf::from(to))? {
                let s = &row.salvaged;
                println!(
                    "{}: {} ({} patches{}{})",
                    row.step_id,
                    row.dest.display(),
                    s.patches.len(),
                    if s.diff.is_some() { ", diff" } else { "" },
                    if s.untracked.is_some() {
                        ", untracked"
                    } else {
                        ""
                    }
                );
            }
        }
        "gc" => {
            args.finish()?;
            for row in worktrees::gc(&ctx)? {
                match (&row.salvage_dir, row.removed) {
                    (Some(d), true) => {
                        println!("{}: removed, salvaged to {}", row.step_id, d.display())
                    }
                    (None, true) => println!("{}: removed", row.step_id),
                    (_, false) => println!("{}: kept (salvage failed)", row.step_id),
                }
            }
        }
        "final" => {
            args.finish()?;
            match Runner::new(&ctx, CheckRunner::discover()).run_final()? {
                CheckOutcome::Green => println!("final: green"),
                CheckOutcome::Red { command, tail } => {
                    println!("final: red: {command}\n{tail}");
                    return Ok(1);
                }
            }
        }
        other => bail!("unknown command `{other}`\n\n{USAGE}"),
    }
    Ok(0)
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("crew-queue: {e:#}");
            ExitCode::from(2)
        }
    }
}
