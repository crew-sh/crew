mod args;

use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, ExitCode};
use std::time::Duration;

use args::Sub;
use crew_usher_client::{Client, ClientError, top};

const EXIT_ERROR: u8 = 125;
const EXIT_REFUSED: u8 = 75;

const USAGE: &str = "\
usage: crew <command> [args]

  run [--kind build|test|harness|game|bench|lsp|other] [--mem 8G] [--cpus N] [--display] -- CMD...
                       run CMD under the usher's admission control
  top [--watch]        running and queued jobs, and what each queued job waits on
  owner here|away|auto tell the usher whether the owner is at the machine
  usher                run the admission daemon in the foreground
  <name>               any other name runs the `crew-<name>` program found beside crew or on PATH

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

fn fail(e: &ClientError) -> ExitCode {
    eprintln!("crew: {e}");
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
            match client.run(&opts) {
                Ok(code) => ExitCode::from(u8::try_from(code & 0xff).unwrap_or(EXIT_ERROR)),
                Err(e) => fail(&e),
            }
        }
        Sub::Top { watch } => loop {
            match client.top() {
                Ok(reply) => {
                    if watch {
                        print!("\x1b[2J\x1b[H");
                    }
                    print!("{}", top::render(&reply));
                }
                Err(e) => return fail(&e),
            }
            if !watch {
                return ExitCode::SUCCESS;
            }
            std::thread::sleep(Duration::from_secs(2));
        },
        Sub::Owner(arg) => match client.set_owner(arg) {
            Ok(msg) => {
                println!("{msg}");
                ExitCode::SUCCESS
            }
            Err(e) => fail(&e),
        },
        Sub::Plugin { name, rest } => exec_plugin(&name, &rest),
    }
}
