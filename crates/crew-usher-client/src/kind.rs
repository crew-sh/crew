use std::path::Path;

use crew_proto::JobKind;

enum Match {
    /// First argument that is not a `+toolchain`.
    Subcommand(&'static [&'static str]),
    /// argv[1] ends with one of these paths.
    Script(&'static [&'static str]),
    AnyArg(&'static [&'static str]),
    Always,
}

struct Rule {
    programs: &'static [&'static str],
    when: Match,
    kind: JobKind,
}

const WRAPPERS: &[&str] = &["hz"];

const RULES: &[Rule] = &[
    Rule { programs: &["cargo"], when: Match::Subcommand(&["build", "check", "clippy", "doc"]), kind: JobKind::Build },
    Rule { programs: &["cargo"], when: Match::Subcommand(&["test", "nextest"]), kind: JobKind::Test },
    Rule { programs: &["python3", "python"], when: Match::Script(&["tools/parity.py", "tools/lan_e2e.py"]), kind: JobKind::Harness },
    Rule { programs: &["horizon-client"], when: Match::Always, kind: JobKind::Game },
    Rule { programs: &["horizon-ctl"], when: Match::AnyArg(&["launch"]), kind: JobKind::Game },
];

fn program_name(arg: &str) -> &str {
    Path::new(arg).file_name().and_then(|n| n.to_str()).unwrap_or(arg)
}

fn matches(when: &Match, args: &[String]) -> bool {
    match when {
        Match::Always => true,
        Match::AnyArg(names) => args.iter().any(|a| names.contains(&a.as_str())),
        Match::Subcommand(names) => args.iter().find(|a| !a.starts_with('+')).is_some_and(|a| names.contains(&a.as_str())),
        Match::Script(suffixes) => args.first().is_some_and(|a| suffixes.iter().any(|s| Path::new(a).ends_with(s))),
    }
}

pub fn infer(argv: &[String]) -> JobKind {
    let mut argv = argv;
    while argv.first().is_some_and(|a| WRAPPERS.contains(&program_name(a))) {
        argv = &argv[1..];
    }
    let Some(program) = argv.first().map(|a| program_name(a)) else { return JobKind::Other };
    RULES
        .iter()
        .find(|r| r.programs.contains(&program) && matches(&r.when, &argv[1..]))
        .map_or(JobKind::Other, |r| r.kind)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind(cmd: &str) -> JobKind {
        infer(&cmd.split_whitespace().map(str::to_string).collect::<Vec<_>>())
    }

    #[test]
    fn the_table() {
        let cases = [
            ("cargo build --release", JobKind::Build),
            ("cargo check -p x", JobKind::Build),
            ("cargo clippy --all-targets", JobKind::Build),
            ("cargo doc", JobKind::Build),
            ("cargo +nightly build", JobKind::Build),
            ("/home/u/.cargo/bin/cargo test", JobKind::Test),
            ("cargo nextest run", JobKind::Test),
            ("hz cargo clippy", JobKind::Build),
            ("/home/umceko/hz-dev/hz cargo nextest run", JobKind::Test),
            ("cargo fmt --check", JobKind::Other),
            ("cargo", JobKind::Other),
            ("python3 tools/parity.py mob_ai", JobKind::Harness),
            ("python3 ./tools/lan_e2e.py --check chest", JobKind::Harness),
            ("python tools/parity.py", JobKind::Harness),
            ("python3 tools/other.py", JobKind::Other),
            ("python3 parity.py", JobKind::Other),
            ("horizon-client --world W", JobKind::Game),
            ("target/fast/horizon-client", JobKind::Game),
            ("horizon-ctl --socket s launch --world W", JobKind::Game),
            ("horizon-ctl state", JobKind::Other),
            ("ls -l", JobKind::Other),
            ("true", JobKind::Other),
        ];
        for (cmd, want) in cases {
            assert_eq!(kind(cmd), want, "{cmd}");
        }
        assert_eq!(infer(&[]), JobKind::Other);
    }
}
