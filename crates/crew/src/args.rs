use std::time::Duration;

use crew_proto::{JobKind, WaitCondition};
use crew_usher_client::{OwnerArg, RunOptions, WaitSpec};

#[derive(Debug, PartialEq)]
pub enum Sub {
    Help,
    Run(RunOptions),
    Top { watch: bool, json: bool },
    Owner { arg: OwnerArg, json: bool },
    Wait { spec: WaitSpec, json: bool },
    Plugin { name: String, rest: Vec<String> },
}

/// "90", "90s", "500ms", "5m", "2h".
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let bad = || format!("bad duration `{s}`");
    let t = s.trim();
    let end = t
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(t.len());
    let n: f64 = t[..end].parse().map_err(|_| bad())?;
    let secs = match &t[end..] {
        "" | "s" => n,
        "ms" => n / 1000.0,
        "m" => n * 60.0,
        "h" => n * 3600.0,
        _ => return Err(bad()),
    };
    Duration::try_from_secs_f64(secs).map_err(|_| bad())
}

fn parse_wait(args: &[String]) -> Result<Sub, String> {
    const USAGE: &str =
        "usage: crew wait queue-empty|owner-away|job <id> [--timeout 30s|5m] [--json]";
    let mut json = false;
    let mut timeout = None;
    let mut words = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--json" => json = true,
            "--timeout" => timeout = Some(parse_duration(value(&mut it, "--timeout")?)?),
            s if s.starts_with('-') => return Err(format!("unknown option `{s}`\n{USAGE}")),
            _ => words.push(a.as_str()),
        }
    }
    let (condition, job_id) = match words.as_slice() {
        ["queue-empty"] => (WaitCondition::QueueEmpty, String::new()),
        ["owner-away"] => (WaitCondition::OwnerAway, String::new()),
        ["job", id] | ["job-granted", id] => (WaitCondition::JobGranted, (*id).to_string()),
        _ => return Err(USAGE.into()),
    };
    Ok(Sub::Wait {
        spec: WaitSpec {
            condition,
            job_id,
            timeout,
        },
        json,
    })
}

/// "8G", "512M", "1.5GiB", "4096" (bytes).
pub fn parse_size(s: &str) -> Result<u64, String> {
    let lower = s.trim().to_ascii_lowercase();
    let digits_end = lower
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(lower.len());
    let (num, unit) = lower.split_at(digits_end);
    let n: f64 = num.parse().map_err(|_| format!("bad size `{s}`"))?;
    let mult: u64 = match unit.trim_end_matches('b').trim_end_matches('i') {
        "" => 1,
        "k" => 1 << 10,
        "m" => 1 << 20,
        "g" => 1 << 30,
        "t" => 1 << 40,
        _ => return Err(format!("bad size unit in `{s}`")),
    };
    Ok((n * mult as f64) as u64)
}

fn value<'a>(it: &mut std::slice::Iter<'a, String>, flag: &str) -> Result<&'a String, String> {
    it.next().ok_or_else(|| format!("{flag} needs a value"))
}

fn parse_run(args: &[String]) -> Result<RunOptions, String> {
    let mut opts = RunOptions::default();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--" => {
                opts.argv = it.cloned().collect();
                break;
            }
            "--kind" => {
                let v = value(&mut it, "--kind")?;
                opts.kind =
                    Some(JobKind::from_name(v).ok_or_else(|| format!("unknown kind `{v}`"))?);
            }
            "--mem" => opts.mem_bytes = Some(parse_size(value(&mut it, "--mem")?)?),
            "--cpus" => {
                let v = value(&mut it, "--cpus")?;
                opts.cpus = Some(v.parse().map_err(|_| format!("bad --cpus `{v}`"))?);
            }
            "--display" => opts.display = true,
            "--json" => opts.json = true,
            s if s.starts_with('-') => {
                return Err(format!("unknown option `{s}` (put the command after `--`)"));
            }
            _ => {
                opts.argv.push(a.clone());
                opts.argv.extend(it.cloned());
                break;
            }
        }
    }
    if opts.argv.is_empty() {
        return Err("run needs a command after `--`".into());
    }
    Ok(opts)
}

pub fn parse(argv: &[String]) -> Result<Sub, String> {
    Ok(match argv.first().map(String::as_str) {
        None | Some("-h" | "--help" | "help") => Sub::Help,
        Some("run") => Sub::Run(parse_run(&argv[1..])?),
        Some("top") => {
            let flags = &argv[1..];
            let json = flags.iter().any(|f| f == "--json");
            let watch = flags.iter().any(|f| f == "--watch" || f == "-w");
            if flags
                .iter()
                .any(|f| !matches!(f.as_str(), "--json" | "--watch" | "-w"))
            {
                return Err("usage: crew top [--watch] [--json]".into());
            }
            Sub::Top { watch, json }
        }
        Some("owner") => {
            let json = argv.iter().skip(1).any(|f| f == "--json");
            let words: Vec<&str> = argv[1..]
                .iter()
                .map(String::as_str)
                .filter(|f| *f != "--json")
                .collect();
            let arg = match words.as_slice() {
                ["here"] => OwnerArg::Here,
                ["away"] => OwnerArg::Away,
                ["auto"] => OwnerArg::Auto,
                _ => return Err("usage: crew owner here|away|auto [--json]".into()),
            };
            Sub::Owner { arg, json }
        }
        Some("wait") => parse_wait(&argv[1..])?,
        // crew-depot serves both, as `crew-depot cargo ...` and `crew-depot build ...`.
        Some(name @ ("cargo" | "build")) => Sub::Plugin {
            name: "depot".to_string(),
            rest: std::iter::once(name.to_string())
                .chain(argv[1..].iter().cloned())
                .collect(),
        },
        Some(name) => Sub::Plugin {
            name: name.to_string(),
            rest: argv[1..].to_vec(),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(cmd: &str) -> Result<Sub, String> {
        super::parse(
            &cmd.split_whitespace()
                .map(str::to_string)
                .collect::<Vec<_>>(),
        )
    }

    fn run(cmd: &str) -> RunOptions {
        match parse(cmd).unwrap() {
            Sub::Run(o) => o,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size("8G").unwrap(), 8 << 30);
        assert_eq!(parse_size("512m").unwrap(), 512 << 20);
        assert_eq!(parse_size("1.5GiB").unwrap(), 3 << 29);
        assert_eq!(parse_size("4096").unwrap(), 4096);
        assert!(parse_size("lots").is_err() && parse_size("8X").is_err());
    }

    #[test]
    fn run_flags_and_command() {
        let o = run("run --kind test --mem 8G --cpus 4 --display -- cargo test -p x");
        assert_eq!(
            (o.kind, o.mem_bytes, o.cpus, o.display),
            (Some(JobKind::Test), Some(8 << 30), Some(4), true)
        );
        assert_eq!(o.argv, ["cargo", "test", "-p", "x"]);
        assert_eq!(run("run -- sh -c exit").argv, ["sh", "-c", "exit"]);
        assert_eq!(run("run true --kind").argv, ["true", "--kind"]);
    }

    #[test]
    fn json_flag_belongs_to_crew_only_before_the_command() {
        assert!(run("run --json -- true").json);
        assert_eq!(run("run -- sh --json").argv, ["sh", "--json"]);
        assert!(!run("run -- sh --json").json);
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("90").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_duration("5m").unwrap(), Duration::from_secs(300));
        assert_eq!(parse_duration("1.5h").unwrap(), Duration::from_secs(5400));
        assert!(
            parse_duration("soon").is_err()
                && parse_duration("5x").is_err()
                && parse_duration("").is_err()
        );
    }

    #[test]
    fn wait_conditions() {
        let wait = |c: &str| match parse(c).unwrap() {
            Sub::Wait { spec, json } => (spec, json),
            other => panic!("{other:?}"),
        };
        let (s, j) = wait("wait queue-empty --timeout 30s --json");
        assert_eq!(
            (s.condition, s.timeout, j),
            (
                WaitCondition::QueueEmpty,
                Some(Duration::from_secs(30)),
                true
            )
        );
        assert_eq!(
            wait("wait owner-away").0.condition,
            WaitCondition::OwnerAway
        );
        let (s, _) = wait("wait job abc-123");
        assert_eq!(
            (s.condition, s.job_id.as_str(), s.timeout),
            (WaitCondition::JobGranted, "abc-123", None)
        );
        assert!(
            parse("wait").is_err() && parse("wait job").is_err() && parse("wait sometime").is_err()
        );
        assert!(
            parse("wait queue-empty --timeout").is_err()
                && parse("wait queue-empty --nope").is_err()
        );
    }

    #[test]
    fn run_errors() {
        assert!(parse("run").is_err());
        assert!(parse("run --kind nope -- true").is_err());
        assert!(parse("run --bogus -- true").is_err());
        assert!(parse("run --mem").is_err());
    }

    #[test]
    fn other_subcommands() {
        assert_eq!(
            parse("top").unwrap(),
            Sub::Top {
                watch: false,
                json: false
            }
        );
        assert_eq!(
            parse("top --watch").unwrap(),
            Sub::Top {
                watch: true,
                json: false
            }
        );
        assert_eq!(
            parse("top --json -w").unwrap(),
            Sub::Top {
                watch: true,
                json: true
            }
        );
        assert!(parse("top --bogus").is_err());
        assert_eq!(
            parse("owner away").unwrap(),
            Sub::Owner {
                arg: OwnerArg::Away,
                json: false
            }
        );
        assert_eq!(
            parse("owner --json here").unwrap(),
            Sub::Owner {
                arg: OwnerArg::Here,
                json: true
            }
        );
        assert!(parse("owner").is_err() && parse("owner sideways").is_err());
        assert_eq!(
            parse("queue merge --now").unwrap(),
            Sub::Plugin {
                name: "queue".into(),
                rest: vec!["merge".into(), "--now".into()]
            }
        );
        assert_eq!(
            parse("cargo build -p x").unwrap(),
            Sub::Plugin {
                name: "depot".into(),
                rest: vec!["cargo".into(), "build".into(), "-p".into(), "x".into()]
            }
        );
        assert_eq!(
            parse("build stats --json").unwrap(),
            Sub::Plugin {
                name: "depot".into(),
                rest: vec!["build".into(), "stats".into(), "--json".into()]
            }
        );
        assert_eq!(
            parse("usher").unwrap(),
            Sub::Plugin {
                name: "usher".into(),
                rest: vec![]
            }
        );
    }
}
