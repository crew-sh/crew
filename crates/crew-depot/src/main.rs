use std::process::ExitCode;

use crew_depot::app::{self, EXIT_ERROR};
use crew_depot::cli::{self, Command, USAGE};

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let command = match cli::parse(&argv) {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("crew: {msg}\n\n{USAGE}");
            return ExitCode::from(EXIT_ERROR);
        }
    };
    let result = match command {
        Command::Help => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Command::Cargo(args) => app::cargo(&args),
        Command::Stats { json } => app::build_stats(json).map(|()| ExitCode::SUCCESS),
        Command::Why { krate, json } => app::build_why(&krate, json).map(|()| ExitCode::SUCCESS),
        Command::Report(args) => app::build_report(&args).map(|()| ExitCode::SUCCESS),
    };
    result.unwrap_or_else(|e| {
        eprintln!("crew: {e:#}");
        ExitCode::from(EXIT_ERROR)
    })
}
