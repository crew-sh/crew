mod args;

use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, ExitCode};
use std::time::Duration;

use args::Sub;
use crew_proto::WaitOutcome;
use crew_usher_client::{Client, ClientError, json, top};

const EXIT_ERROR: u8 = 125;
const EXIT_REFUSED: u8 = 75;
const EXIT_TIMEOUT: u8 = 124;
const EXIT_NEVER_GRANTED: u8 = 2;

const USAGE: &str = "\
usage: crew <command> [args]

  run [--kind build|test|harness|game|bench|lsp|other] [--mem 8G] [--cpus N] [--display] -- CMD...
                       run CMD under the usher's admission control
  top [--watch]        running and queued jobs, what each queued job waits on, agent sessions and projects
  owner here|away|auto tell the usher whether the owner is at the machine
  wait queue-empty|owner-away|job <id> [--timeout 30s|5m]
                       block until the usher reports the condition (exit 0), 124 on timeout, 2 if the job never ran
  usher                run the admission daemon in the foreground
  <name>               any other name runs the `crew-<name>` program found beside crew or on PATH

--json (run, top, owner, wait) prints typed JSON: top, owner and wait on stdout; run's progress events on stderr, one per line.
Plugins (`crew git`, `crew queue`...) take their own flags.

CREW_STRICT=1 makes `crew run` fail when the usher is not running instead of running unmanaged.";

fn plugin_path(name: &str) -> PathBuf {
    let beside = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(name)));
    match beside {
        Some(p) if p.exists() => p,
        _ => PathBuf::from(name),
    }
}

fn exec_plugin(name: &str, rest: &[String]) -> ExitCode {
    let program = format!("crew-{name}");
    let err = Command::new(plugin_path(&program)).args(rest).exec();
    eprintln!("crew: cannot run `{program}`: {err}");
    ExitCode::from(EXIT_ERROR)
}

fn fail(e: &ClientError, as_json: bool) -> ExitCode {
    if as_json {
        eprintln!("{}", json::failure(e));
    } else {
        eprintln!("crew: {e}");
    }
    ExitCode::from(if matches!(e, ClientError::Refused { .. }) {
        EXIT_REFUSED
    } else {
        EXIT_ERROR
    })
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let sub = match args::parse(&argv) {
        Ok(s) => s,
        Err(msg) => {
            eprintln!("crew: {msg}\n\n{USAGE}");
            return ExitCode::from(EXIT_ERROR);
        }
    };
    let client = Client::from_env();
    match sub {
        Sub::Help => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        Sub::Run(mut opts) => {
            opts.strict = std::env::var("CREW_STRICT").is_ok_and(|v| v == "1");
            opts.inside_job = std::env::var_os("CREW_JOB").is_some();
            match client.run(&opts) {
                Ok(code) => ExitCode::from(u8::try_from(code & 0xff).unwrap_or(EXIT_ERROR)),
                Err(e) => fail(&e, opts.json),
            }
        }
        Sub::Top { watch, json } => loop {
            match client.top() {
                Ok(reply) if json => println!("{}", json::top(&reply)),
                Ok(reply) => {
                    if watch {
                        print!("\x1b[2J\x1b[H");
                    }
                    print!("{}", top::render(&reply));
                }
                Err(e) => return fail(&e, json),
            }
            if !watch {
                return ExitCode::SUCCESS;
            }
            std::thread::sleep(Duration::from_secs(2));
        },
        Sub::Owner { arg, json } => match client.set_owner(arg) {
            Ok(msg) => {
                if json {
                    println!("{}", json::owner(&msg));
                } else {
                    println!("{msg}");
                }
                ExitCode::SUCCESS
            }
            Err(e) => fail(&e, json),
        },
        Sub::Wait { spec, json } => match client.wait(&spec) {
            Ok(reply) => {
                if json {
                    println!("{}", json::wait(&reply));
                } else {
                    println!("{}", reply.detail);
                }
                match WaitOutcome::try_from(reply.outcome).unwrap_or(WaitOutcome::Unspecified) {
                    WaitOutcome::Satisfied => ExitCode::SUCCESS,
                    WaitOutcome::TimedOut => ExitCode::from(EXIT_TIMEOUT),
                    WaitOutcome::NeverGranted => ExitCode::from(EXIT_NEVER_GRANTED),
                    WaitOutcome::Rejected | WaitOutcome::Unspecified => ExitCode::from(EXIT_ERROR),
                }
            }
            Err(e) => fail(&e, json),
        },
        Sub::Plugin { name, rest } => exec_plugin(&name, &rest),
    }
}
