use std::fs;
use std::io;
use std::path::Path;

use crew_proto::JobKind;
use serde::Deserialize;

enum Match {
    /// First argument that is not a `+toolchain`.
    Subcommand(&'static [&'static str]),
    /// The leading arguments, in order.
    Args(&'static [&'static str]),
    Always,
}

struct Rule {
    programs: &'static [&'static str],
    when: Match,
    kind: JobKind,
}

const RULES: &[Rule] = &[
    Rule {
        programs: &["cargo"],
        when: Match::Subcommand(&["build", "check", "clippy", "doc"]),
        kind: JobKind::Build,
    },
    Rule {
        programs: &["cargo"],
        when: Match::Subcommand(&["test", "nextest"]),
        kind: JobKind::Test,
    },
    Rule {
        programs: &["go"],
        when: Match::Subcommand(&["build", "vet"]),
        kind: JobKind::Build,
    },
    Rule {
        programs: &["go"],
        when: Match::Subcommand(&["test"]),
        kind: JobKind::Test,
    },
    Rule {
        programs: &["npm", "pnpm"],
        when: Match::Subcommand(&["test"]),
        kind: JobKind::Test,
    },
    Rule {
        programs: &["pytest"],
        when: Match::Always,
        kind: JobKind::Test,
    },
    Rule {
        programs: &["python3", "python"],
        when: Match::Args(&["-m", "pytest"]),
        kind: JobKind::Test,
    },
    Rule {
        programs: &["make"],
        when: Match::Always,
        kind: JobKind::Build,
    },
];

#[derive(Debug, thiserror::Error)]
pub enum ProjectRulesError {
    #[error("reading {path}: {source}")]
    Read { path: String, source: io::Error },
    #[error("parsing [usher] in {path}: {message}")]
    Parse { path: String, message: String },
}

/// Argv prefixes per job kind, each written as one command string: `"my-ctl launch"`.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UsherKinds {
    pub build: Vec<String>,
    pub test: Vec<String>,
    pub harness: Vec<String>,
    pub game: Vec<String>,
    pub bench: Vec<String>,
    pub lsp: Vec<String>,
    pub other: Vec<String>,
}

/// The `[usher]` table of crew.toml.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UsherTable {
    pub kinds: UsherKinds,
    /// Programs that run their arguments as a command (`time`, a repo's own `./run`): skipped before matching.
    pub wrappers: Vec<String>,
}

/// Only `[usher]` is ours; every other table in crew.toml belongs to other tools.
#[derive(Deserialize)]
struct CrewToml {
    usher: Option<UsherTable>,
}

struct PrefixRule {
    prefix: Vec<String>,
    kind: JobKind,
}

/// A repo's own job-kind rules, checked before the built-in table.
#[derive(Default)]
pub struct ProjectRules {
    rules: Vec<PrefixRule>,
    wrappers: Vec<String>,
}

fn program_name(arg: &str) -> &str {
    Path::new(arg)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(arg)
}

fn same_arg(have: &str, want: &str) -> bool {
    have.strip_prefix("./").unwrap_or(have) == want.strip_prefix("./").unwrap_or(want)
}

impl PrefixRule {
    fn matches(&self, argv: &[String]) -> bool {
        let Some((first, rest)) = self.prefix.split_first() else {
            return false;
        };
        let Some((program, args)) = argv.split_first() else {
            return false;
        };
        let program_ok = if first.contains('/') {
            program == first
        } else {
            program_name(program) == first
        };
        program_ok
            && args.len() >= rest.len()
            && rest
                .iter()
                .zip(args)
                .all(|(want, have)| same_arg(have, want))
    }
}

impl ProjectRules {
    /// Parses the `[usher]` table of a crew.toml text; no table means no rules.
    pub fn parse(text: &str, shown_path: &str) -> Result<Self, ProjectRulesError> {
        let bad = |message: String| ProjectRulesError::Parse {
            path: shown_path.to_string(),
            message,
        };
        let file: CrewToml = toml::from_str(text).map_err(|e| bad(e.to_string()))?;
        let table = file.usher.unwrap_or_default();
        let k = table.kinds;
        let mut rules = Vec::new();
        for (kind, commands) in [
            (JobKind::Build, k.build),
            (JobKind::Test, k.test),
            (JobKind::Harness, k.harness),
            (JobKind::Game, k.game),
            (JobKind::Bench, k.bench),
            (JobKind::Lsp, k.lsp),
            (JobKind::Other, k.other),
        ] {
            for command in commands {
                let prefix: Vec<String> = command.split_whitespace().map(str::to_string).collect();
                if prefix.is_empty() {
                    return Err(bad(format!("kinds.{} has an empty command", kind.name())));
                }
                rules.push(PrefixRule { prefix, kind });
            }
        }
        Ok(Self {
            rules,
            wrappers: table.wrappers,
        })
    }

    /// Rules of the nearest crew.toml at or above `dir`; none when there is no such file.
    pub fn discover(dir: &Path) -> Result<Self, ProjectRulesError> {
        let Some(path) = dir
            .ancestors()
            .map(|d| d.join("crew.toml"))
            .find(|p| p.is_file())
        else {
            return Ok(Self::default());
        };
        let shown = path.display().to_string();
        let text = fs::read_to_string(&path).map_err(|source| ProjectRulesError::Read {
            path: shown.clone(),
            source,
        })?;
        Self::parse(&text, &shown)
    }

    fn strip_wrappers<'a>(&self, mut argv: &'a [String]) -> &'a [String] {
        while argv
            .first()
            .is_some_and(|a| self.wrappers.iter().any(|w| w == program_name(a)))
        {
            argv = &argv[1..];
        }
        argv
    }

    /// The longest matching prefix wins; kinds earlier in the table win a tie.
    fn kind_of(&self, argv: &[String]) -> Option<JobKind> {
        self.rules
            .iter()
            .filter(|r| r.matches(argv))
            .reduce(|best, r| {
                if r.prefix.len() > best.prefix.len() {
                    r
                } else {
                    best
                }
            })
            .map(|r| r.kind)
    }
}

fn builtin_matches(when: &Match, args: &[String]) -> bool {
    match when {
        Match::Always => true,
        Match::Args(want) => args.len() >= want.len() && want.iter().zip(args).all(|(w, a)| w == a),
        Match::Subcommand(names) => args
            .iter()
            .find(|a| !a.starts_with('+'))
            .is_some_and(|a| names.contains(&a.as_str())),
    }
}

/// The repo's rules first, then the built-in table; anything unmatched is `Other`.
pub fn infer_with(argv: &[String], project: &ProjectRules) -> JobKind {
    let argv = project.strip_wrappers(argv);
    if let Some(kind) = project.kind_of(argv) {
        return kind;
    }
    let Some(program) = argv.first().map(|a| program_name(a)) else {
        return JobKind::Other;
    };
    RULES
        .iter()
        .find(|r| r.programs.contains(&program) && builtin_matches(&r.when, &argv[1..]))
        .map_or(JobKind::Other, |r| r.kind)
}

/// The built-in table alone.
pub fn infer(argv: &[String]) -> JobKind {
    infer_with(argv, &ProjectRules::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(cmd: &str) -> Vec<String> {
        cmd.split_whitespace().map(str::to_string).collect()
    }

    fn kind(cmd: &str) -> JobKind {
        infer(&words(cmd))
    }

    #[test]
    fn the_builtin_table() {
        let cases = [
            ("cargo build --release", JobKind::Build),
            ("cargo check -p x", JobKind::Build),
            ("cargo clippy --all-targets", JobKind::Build),
            ("cargo doc", JobKind::Build),
            ("cargo +nightly build", JobKind::Build),
            ("/usr/local/bin/cargo test", JobKind::Test),
            ("cargo nextest run", JobKind::Test),
            ("cargo fmt --check", JobKind::Other),
            ("cargo", JobKind::Other),
            ("go build ./...", JobKind::Build),
            ("go vet ./...", JobKind::Build),
            ("go test ./...", JobKind::Test),
            ("go run main.go", JobKind::Other),
            ("npm test", JobKind::Test),
            ("pnpm test", JobKind::Test),
            ("npm install", JobKind::Other),
            ("pytest -x tests", JobKind::Test),
            ("python3 -m pytest", JobKind::Test),
            ("python3 script.py", JobKind::Other),
            ("make -j8", JobKind::Build),
            ("ls -l", JobKind::Other),
            ("true", JobKind::Other),
        ];
        for (cmd, want) in cases {
            assert_eq!(kind(cmd), want, "{cmd}");
        }
        assert_eq!(infer(&[]), JobKind::Other);
    }

    #[test]
    fn no_project_specific_program_is_built_in() {
        for cmd in [
            "my-game --world W",
            "python3 tools/check.py",
            "wrap cargo build",
        ] {
            assert_eq!(kind(cmd), JobKind::Other, "{cmd}");
        }
    }

    const PROJECT: &str = r#"
[plan]
file = "plan.toml"
request = "r"

[usher]
wrappers = ["wrap"]

[usher.kinds]
game = ["my-game", "my-ctl launch"]
harness = ["python3 tools/check.py", "python3 tools/e2e.py"]
test = ["cargo test -p slow"]
"#;

    fn project() -> ProjectRules {
        ProjectRules::parse(PROJECT, "crew.toml").unwrap()
    }

    fn kind_in(cmd: &str) -> JobKind {
        infer_with(&words(cmd), &project())
    }

    #[test]
    fn project_prefixes_classify_commands() {
        let cases = [
            ("my-game --world W", JobKind::Game),
            ("target/fast/my-game", JobKind::Game),
            ("my-ctl launch --world W", JobKind::Game),
            ("my-ctl state", JobKind::Other),
            ("my-ctl --socket s launch", JobKind::Other),
            ("python3 tools/check.py unit_a", JobKind::Harness),
            ("python3 ./tools/e2e.py --check x", JobKind::Harness),
            ("python3 tools/other.py", JobKind::Other),
            ("python3 tools/check.pyc", JobKind::Other),
        ];
        for (cmd, want) in cases {
            assert_eq!(kind_in(cmd), want, "{cmd}");
        }
    }

    #[test]
    fn project_rules_come_before_the_builtins_and_the_longest_prefix_wins() {
        assert_eq!(kind_in("cargo test -p slow --release"), JobKind::Test);
        let rules = ProjectRules::parse(
            "[usher.kinds]\nbench = [\"cargo bench\"]\nbuild = [\"cargo\"]\ntest = [\"cargo test\"]\n",
            "crew.toml",
        )
        .unwrap();
        let k = |c: &str| infer_with(&words(c), &rules);
        assert_eq!(k("cargo bench"), JobKind::Bench);
        assert_eq!(k("cargo test"), JobKind::Test);
        assert_eq!(k("cargo run"), JobKind::Build);
        assert_eq!(k("cargo nextest run"), JobKind::Build);
    }

    #[test]
    fn project_wrappers_are_skipped_before_matching() {
        assert_eq!(kind_in("wrap cargo clippy"), JobKind::Build);
        assert_eq!(kind_in("/opt/bin/wrap wrap my-game"), JobKind::Game);
        assert_eq!(kind("wrap cargo clippy"), JobKind::Other);
    }

    #[test]
    fn a_file_without_an_usher_table_adds_nothing() {
        let rules = ProjectRules::parse(
            "[plan]\nfile = \"p.toml\"\n[queue]\nanything = 1\n",
            "crew.toml",
        )
        .unwrap();
        assert_eq!(infer_with(&words("my-game"), &rules), JobKind::Other);
        assert_eq!(infer_with(&words("cargo build"), &rules), JobKind::Build);
    }

    #[test]
    fn unknown_keys_are_rejected_only_inside_usher() {
        let bad = [
            "[usher]\nkind = {}\n",
            "[usher.kinds]\ngames = [\"x\"]\n",
            "[usher.kinds]\ngame = \"x\"\n",
            "[usher.kinds]\ngame = [\"\"]\n",
            "[usher.kinds]\ngame = [\"  \"]\n",
        ];
        for text in bad {
            let e = ProjectRules::parse(text, "/r/crew.toml").err();
            assert!(
                e.is_some_and(|e| e.to_string().contains("/r/crew.toml")),
                "{text}"
            );
        }
        assert!(ProjectRules::parse("[queue]\nwhatever = 1\n[other]\nx = 2\n", "p").is_ok());
    }

    #[test]
    fn discover_walks_up_to_the_nearest_crew_toml() {
        let dir = tempfile::tempdir().unwrap();
        let deep = dir.path().join("a/b");
        fs::create_dir_all(&deep).unwrap();
        let none = ProjectRules::discover(&deep).unwrap();
        assert_eq!(infer_with(&words("my-game"), &none), JobKind::Other);
        fs::write(dir.path().join("crew.toml"), PROJECT).unwrap();
        let found = ProjectRules::discover(&deep).unwrap();
        assert_eq!(infer_with(&words("my-game"), &found), JobKind::Game);
        fs::write(
            dir.path().join("a/crew.toml"),
            "[usher.kinds]\nbench = [\"my-game\"]\n",
        )
        .unwrap();
        let nearer = ProjectRules::discover(&deep).unwrap();
        assert_eq!(infer_with(&words("my-game"), &nearer), JobKind::Bench);
        fs::write(dir.path().join("a/crew.toml"), "[usher]\nnope = 1\n").unwrap();
        assert!(ProjectRules::discover(&deep).is_err());
    }
}
