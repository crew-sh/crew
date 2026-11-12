use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

const TAIL_LINES: usize = 60;
/// `crew run` exit codes for "refused" and "error": the check never ran, so nobody is to blame.
const CREW_REFUSED: i32 = 75;
const CREW_ERROR: i32 = 125;
/// A job reports 128 + the signal that ended it: the OOM killer's SIGKILL, `crew cancel`'s SIGTERM.
const KILLED: i32 = 137;
const TERMINATED: i32 = 143;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckOutcome {
    Green,
    Red { command: String, tail: String },
}

/// Runs check commands, through `crew run --kind build` when a crew binary is known.
#[derive(Debug, Clone, Default)]
pub struct CheckRunner {
    pub crew_bin: Option<PathBuf>,
}

impl CheckRunner {
    /// `CREW_BIN`, else a `crew` beside this executable, else one on PATH, else none (commands run directly).
    pub fn discover() -> Self {
        if let Some(p) = std::env::var_os("CREW_BIN") {
            return Self {
                crew_bin: Some(PathBuf::from(p)),
            };
        }
        let beside = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("crew")))
            .filter(|p| p.is_file());
        let on_path = std::env::var_os("PATH").and_then(|path| {
            std::env::split_paths(&path)
                .map(|d| d.join("crew"))
                .find(|p| p.is_file())
        });
        Self {
            crew_bin: beside.or(on_path),
        }
    }

    pub fn run_all(&self, dir: &Path, commands: &[String]) -> Result<CheckOutcome> {
        for command in commands {
            // A check that was killed or never started says nothing about the code: once more, then give up.
            let mut attempt = self.run_one(dir, command)?;
            if matches!(attempt, Attempt::DidNotRun(_)) {
                attempt = self.run_one(dir, command)?;
            }
            match attempt {
                Attempt::Passed => {}
                Attempt::Failed(tail) => {
                    return Ok(CheckOutcome::Red {
                        command: command.clone(),
                        tail,
                    });
                }
                Attempt::DidNotRun(why) => {
                    anyhow::bail!("the check `{command}` did not run to a result, twice: {why}");
                }
            }
        }
        Ok(CheckOutcome::Green)
    }

    fn run_one(&self, dir: &Path, command: &str) -> Result<Attempt> {
        {
            let wrapped = format!("exec 2>&1; {command}");
            let mut cmd = match &self.crew_bin {
                Some(crew) => {
                    let mut c = Command::new(crew);
                    c.args(["run", "--kind", "build", "--", "sh", "-c", &wrapped]);
                    c
                }
                None => {
                    let mut c = Command::new("sh");
                    c.args(["-c", &wrapped]);
                    c
                }
            };
            cmd.current_dir(dir);
            if std::env::var_os("CREW_AGENT").is_none() {
                cmd.env("CREW_AGENT", "queue");
            }
            let out = cmd
                .output()
                .with_context(|| format!("spawn check `{command}`"))?;
            if out.status.success() {
                return Ok(Attempt::Passed);
            }
            let text = String::from_utf8_lossy(&out.stdout);
            let code = out.status.code();
            let through_crew = self.crew_bin.is_some();
            let infrastructure = code.is_none()
                || (through_crew
                    && matches!(code, Some(CREW_REFUSED | CREW_ERROR | KILLED | TERMINATED)));
            if infrastructure {
                return Ok(Attempt::DidNotRun(format!(
                    "exit {code:?}: {}",
                    tail(&format!("{text}{}", String::from_utf8_lossy(&out.stderr)))
                )));
            }
            Ok(Attempt::Failed(tail(&text)))
        }
    }
}

/// One run of a check command.
enum Attempt {
    Passed,
    Failed(String),
    /// Killed by a signal, or `crew run` refused or failed: the code was never judged.
    DidNotRun(String),
}

pub fn tail(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(TAIL_LINES)..].join("\n")
}
