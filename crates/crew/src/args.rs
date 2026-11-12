use crew_proto::JobKind;
use crew_usher_client::{OwnerArg, RunOptions};

#[derive(Debug, PartialEq)]
pub enum Sub {
    Help,
    Run(RunOptions),
    Top { watch: bool },
    Owner(OwnerArg),
    Plugin { name: String, rest: Vec<String> },
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
        Some("top") => match &argv[1..] {
            [] => Sub::Top { watch: false },
            [f] if f == "--watch" || f == "-w" => Sub::Top { watch: true },
            _ => return Err("usage: crew top [--watch]".into()),
        },
        Some("owner") => match argv.get(1).map(String::as_str) {
            Some("here") if argv.len() == 2 => Sub::Owner(OwnerArg::Here),
            Some("away") if argv.len() == 2 => Sub::Owner(OwnerArg::Away),
            Some("auto") if argv.len() == 2 => Sub::Owner(OwnerArg::Auto),
            _ => return Err("usage: crew owner here|away|auto".into()),
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
    fn run_errors() {
        assert!(parse("run").is_err());
        assert!(parse("run --kind nope -- true").is_err());
        assert!(parse("run --bogus -- true").is_err());
        assert!(parse("run --mem").is_err());
    }

    #[test]
    fn other_subcommands() {
        assert_eq!(parse("top").unwrap(), Sub::Top { watch: false });
        assert_eq!(parse("top --watch").unwrap(), Sub::Top { watch: true });
        assert_eq!(parse("owner away").unwrap(), Sub::Owner(OwnerArg::Away));
        assert!(parse("owner").is_err() && parse("owner sideways").is_err());
        assert_eq!(
            parse("queue merge --now").unwrap(),
            Sub::Plugin {
                name: "queue".into(),
                rest: vec!["merge".into(), "--now".into()]
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
