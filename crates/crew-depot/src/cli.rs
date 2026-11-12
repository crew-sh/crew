use std::path::PathBuf;

pub const USAGE: &str = "\
usage: crew cargo <cargo arguments...>
       crew build stats [--json]
       crew build why <crate> [--json]
       crew build report --shared <other project dir> [--check] [--profile NAME] [--summary] [--json]

  cargo     run cargo through mbx (when installed) under an usher build grant; `[depot]` in
            ~/.config/crew/depot.toml and crew.toml sets the toolchain pin and opt-in profile overrides
  stats     what this project's builds got from the cache: the last build, the sum, mbx's lifetime totals
  why       why <crate> missed the cache in the last recorded build (reads `mbx explain --last`)
  report    for each dependency unit here, whether the other project compiles the same thing,
            or the first reason it does not; reports only, never edits a manifest

CREW_STRICT=1 makes `crew cargo` fail when the usher is not running instead of building unmanaged.";

#[derive(Debug, PartialEq, Eq)]
pub struct ReportArgs {
    pub shared: PathBuf,
    pub check: bool,
    pub profile: Option<String>,
    pub summary: bool,
    pub json: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Help,
    Cargo(Vec<String>),
    Stats { json: bool },
    Why { krate: String, json: bool },
    Report(ReportArgs),
}

fn flags<'a>(args: &'a [String], known: &[&str]) -> Result<(Vec<&'a str>, Vec<&'a str>), String> {
    let (mut on, mut words) = (Vec::new(), Vec::new());
    for a in args {
        if a.starts_with("--") {
            if !known.contains(&a.as_str()) {
                return Err(format!("unknown option `{a}`"));
            }
            on.push(a.as_str());
        } else {
            words.push(a.as_str());
        }
    }
    Ok((on, words))
}

fn parse_report(args: &[String]) -> Result<ReportArgs, String> {
    let (mut shared, mut profile) = (None, None);
    let (mut check, mut summary, mut json) = (false, false, false);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--shared" => {
                shared = Some(
                    it.next()
                        .ok_or("--shared needs a project directory")?
                        .into(),
                )
            }
            "--profile" => profile = Some(it.next().ok_or("--profile needs a name")?.clone()),
            "--check" => check = true,
            "--summary" => summary = true,
            "--json" => json = true,
            other => return Err(format!("unexpected `{other}`")),
        }
    }
    Ok(ReportArgs {
        shared: shared.ok_or("report needs --shared <other project dir>")?,
        check,
        profile,
        summary,
        json,
    })
}

pub fn parse(argv: &[String]) -> Result<Command, String> {
    let Some((first, rest)) = argv.split_first() else {
        return Ok(Command::Help);
    };
    match first.as_str() {
        "-h" | "--help" | "help" => Ok(Command::Help),
        "cargo" if rest.is_empty() => Err("cargo needs arguments: `crew cargo build`".into()),
        "cargo" => Ok(Command::Cargo(rest.to_vec())),
        "build" => parse_build(rest),
        other => Err(format!("unknown command `{other}`")),
    }
}

fn parse_build(rest: &[String]) -> Result<Command, String> {
    let Some((sub, args)) = rest.split_first() else {
        return Err("build needs stats, why or report".into());
    };
    match sub.as_str() {
        "stats" => {
            let (on, words) = flags(args, &["--json"])?;
            if !words.is_empty() {
                return Err("usage: crew build stats [--json]".into());
            }
            Ok(Command::Stats {
                json: !on.is_empty(),
            })
        }
        "why" => {
            let (on, words) = flags(args, &["--json"])?;
            match words.as_slice() {
                [krate] => Ok(Command::Why {
                    krate: (*krate).to_string(),
                    json: !on.is_empty(),
                }),
                _ => Err("usage: crew build why <crate> [--json]".into()),
            }
        }
        "report" => parse_report(args).map(Command::Report),
        other => Err(format!("unknown build command `{other}`")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(cmd: &str) -> Result<Command, String> {
        parse(
            &cmd.split_whitespace()
                .map(str::to_string)
                .collect::<Vec<_>>(),
        )
    }

    #[test]
    fn cargo_keeps_every_argument_including_flags() {
        assert_eq!(
            p("cargo build --release -p x -- --nocapture").unwrap(),
            Command::Cargo(
                "build --release -p x -- --nocapture"
                    .split(' ')
                    .map(String::from)
                    .collect()
            )
        );
        assert!(p("cargo").is_err());
    }

    #[test]
    fn build_subcommands() {
        assert_eq!(p("build stats").unwrap(), Command::Stats { json: false });
        assert_eq!(
            p("build stats --json").unwrap(),
            Command::Stats { json: true }
        );
        assert_eq!(
            p("build why serde --json").unwrap(),
            Command::Why {
                krate: "serde".into(),
                json: true
            }
        );
        assert!(
            p("build why").is_err() && p("build why a b").is_err() && p("build stats x").is_err()
        );
        assert!(
            p("build").is_err() && p("build nope").is_err() && p("build stats --nope").is_err()
        );
    }

    #[test]
    fn report_needs_the_other_project() {
        assert_eq!(
            p("build report --shared /w/other --check --profile fast --summary --json").unwrap(),
            Command::Report(ReportArgs {
                shared: "/w/other".into(),
                check: true,
                profile: Some("fast".into()),
                summary: true,
                json: true
            })
        );
        assert!(p("build report").is_err() && p("build report --shared").is_err());
        assert!(p("build report --shared /w --bogus").is_err());
    }

    #[test]
    fn help_and_unknown() {
        assert_eq!(p("").unwrap(), Command::Help);
        assert_eq!(p("--help").unwrap(), Command::Help);
        assert!(p("frobnicate").is_err());
    }
}
