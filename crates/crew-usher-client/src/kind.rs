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
        programs: &["cargo"],
        when: Match::Subcommand(&["bench", "criterion"]),
        kind: JobKind::Bench,
    },
    Rule {
        programs: &["make"],
        when: Match::Subcommand(&["bench", "benchmark", "benchmarks"]),
        kind: JobKind::Bench,
    },
    Rule {
        programs: &["make"],
        when: Match::Subcommand(&["test", "check"]),
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

/// The checkout directory that holds `.git` for a crew.toml's directory.
fn found_root(dir: &Path) -> &Path {
    dir.ancestors()
        .find(|d| d.join(".git").exists())
        .unwrap_or(dir)
}

/// For a linked worktree, the main checkout it belongs to (from the `.git` file, without running git).
fn main_checkout(dir: &Path) -> Option<std::path::PathBuf> {
    let checkout = found_root(dir);
    let dotgit = checkout.join(".git");
    if !dotgit.is_file() {
        return None;
    }
    let text = fs::read_to_string(&dotgit).ok()?;
    let gitdir = checkout.join(text.strip_prefix("gitdir:")?.trim());
    let common = fs::read_to_string(gitdir.join("commondir")).ok()?;
    let common = gitdir.join(common.trim()).canonicalize().ok()?;
    let main = common.parent()?.to_path_buf();
    (main.canonicalize().ok()? != checkout.canonicalize().ok()?).then_some(main)
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

    /// Rules of the nearest crew.toml above `dir`; in a linked git worktree, the main checkout's file instead.
    pub fn discover(dir: &Path) -> Result<Self, ProjectRulesError> {
        let Some(found) = dir.ancestors().find(|d| d.join("crew.toml").is_file()) else {
            return Ok(Self::default());
        };
        let path = match main_checkout(found) {
            Some(main) => {
                let relative = found
                    .strip_prefix(found_root(found))
                    .unwrap_or(Path::new(""));
                let theirs = main.join(relative).join("crew.toml");
                if theirs.is_file() {
                    theirs
                } else {
                    return Ok(Self::default());
                }
            }
            None => found.join("crew.toml"),
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

/// Options that come before a cargo subcommand and take a value of their own.
const CARGO_VALUE_FLAGS: &[&str] = &["--config", "-Z", "-C", "--color"];

/// The subcommand: the first argument that is not a `+toolchain` or a leading option (and its value).
fn subcommand(args: &[String]) -> Option<&str> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a.starts_with('+') {
            continue;
        }
        if a.starts_with('-') {
            if CARGO_VALUE_FLAGS.contains(&a.as_str()) {
                it.next();
            }
            continue;
        }
        return Some(a);
    }
    None
}

fn builtin_matches(when: &Match, args: &[String]) -> bool {
    match when {
        Match::Always => true,
        Match::Args(want) => args.len() >= want.len() && want.iter().zip(args).all(|(w, a)| w == a),
        Match::Subcommand(names) => subcommand(args).is_some_and(|a| names.contains(&a)),
    }
}

/// Programs that run the rest of their command line, and how many leading words besides the program they take.
fn peel(argv: &[String]) -> Option<Vec<String>> {
    let program = program_name(argv.first()?);
    let rest = &argv[1..];
    let is_assignment = |a: &String| {
        a.split_once('=').is_some_and(|(k, _)| {
            !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        })
    };
    let after = match program {
        "env" => {
            let mut it = rest.iter().peekable();
            while let Some(a) = it.peek() {
                if a.as_str() == "-u" || a.as_str() == "-C" || a.as_str() == "-S" {
                    it.next();
                    it.next();
                } else if a.starts_with('-') || is_assignment(a) {
                    it.next();
                } else {
                    break;
                }
            }
            it.cloned().collect()
        }
        "timeout" => {
            let mut it = rest.iter().peekable();
            while let Some(a) = it.peek() {
                if matches!(a.as_str(), "-s" | "-k" | "--signal" | "--kill-after") {
                    it.next();
                    it.next();
                } else if a.starts_with('-') {
                    it.next();
                } else {
                    break;
                }
            }
            it.next(); // the duration
            it.cloned().collect()
        }
        "nice" | "ionice" | "chrt" | "time" | "nohup" | "setsid" | "stdbuf" => {
            let mut it = rest.iter().peekable();
            while let Some(a) = it.peek() {
                if matches!(a.as_str(), "-n" | "-c" | "-p") {
                    it.next();
                    it.next();
                } else if a.starts_with('-') {
                    it.next();
                } else {
                    break;
                }
            }
            it.cloned().collect()
        }
        // mbx takes cargo's own arguments.
        "mbx" => std::iter::once("cargo".to_string())
            .chain(rest.iter().cloned())
            .collect(),
        _ => return None,
    };
    Some(after)
}

/// How much a kind needs the machine: when a shell line holds several commands, the heaviest decides.
fn weight(kind: JobKind) -> u8 {
    match kind {
        JobKind::Bench => 6,
        JobKind::Game => 5,
        JobKind::Harness => 4,
        JobKind::Build => 3,
        JobKind::Test => 2,
        JobKind::Lsp => 1,
        JobKind::Other | JobKind::Unspecified => 0,
    }
}

/// `sh -c "cargo build && ./x"`: the kind of the heaviest command in the string.
fn shell_line(argv: &[String], project: &ProjectRules, depth: u8) -> Option<JobKind> {
    let program = program_name(argv.first()?);
    if !matches!(program, "sh" | "bash" | "zsh" | "dash") {
        return None;
    }
    let script = argv.get(argv.iter().position(|a| a == "-c")? + 1)?;
    let mut best = JobKind::Other;
    for segment in script.split(['\n', ';', '&', '|']) {
        let words: Vec<String> = segment.split_whitespace().map(str::to_string).collect();
        if words.is_empty() {
            continue;
        }
        let kind = infer_depth(&words, project, depth + 1);
        if weight(kind) > weight(best) {
            best = kind;
        }
    }
    Some(best)
}

fn infer_depth(argv: &[String], project: &ProjectRules, depth: u8) -> JobKind {
    let argv = project.strip_wrappers(argv);
    if let Some(kind) = project.kind_of(argv) {
        return kind;
    }
    if depth < 6 {
        if let Some(inner) = peel(argv) {
            return infer_depth(&inner, project, depth + 1);
        }
        if let Some(kind) = shell_line(argv, project, depth) {
            return kind;
        }
    }
    let Some(program) = argv.first().map(|a| program_name(a)) else {
        return JobKind::Other;
    };
    RULES
        .iter()
        .find(|r| r.programs.contains(&program) && builtin_matches(&r.when, &argv[1..]))
        .map_or(JobKind::Other, |r| r.kind)
}

/// The repo's rules first, then the built-in table, looking through `env`, `timeout`, `nice`, `mbx` and `sh -c`.
pub fn infer_with(argv: &[String], project: &ProjectRules) -> JobKind {
    infer_depth(argv, project, 0)
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
    fn benches_are_benches_whatever_wraps_them() {
        for cmd in [
            "cargo bench",
            "cargo bench -p crew-usher",
            "cargo --locked bench",
            "cargo +nightly bench --bench x",
            "env RUST_LOG=info cargo bench",
            "timeout 600 cargo bench",
            "nice -n 10 cargo bench",
            "mbx bench",
            "make bench",
        ] {
            assert_eq!(kind(cmd), JobKind::Bench, "{cmd}");
        }
        assert_eq!(
            infer(&["sh".into(), "-c".into(), "cargo bench".into()]),
            JobKind::Bench
        );
    }

    #[test]
    fn flags_and_wrappers_do_not_hide_builds_and_tests() {
        for (cmd, want) in [
            ("cargo --locked build", JobKind::Build),
            ("cargo -Z unstable-options build", JobKind::Build),
            ("cargo --config foo=bar test", JobKind::Test),
            ("env X=1 cargo test", JobKind::Test),
            ("env -i X=1 cargo build", JobKind::Build),
            ("timeout 600 cargo test", JobKind::Test),
            ("timeout -s KILL 5m cargo build", JobKind::Build),
            ("nice cargo build", JobKind::Build),
            ("mbx build", JobKind::Build),
            ("mbx nextest run", JobKind::Test),
            ("make test", JobKind::Test),
            ("make -j8", JobKind::Build),
            ("env X=1 true", JobKind::Other),
            ("timeout 5", JobKind::Other),
        ] {
            assert_eq!(kind(cmd), want, "{cmd}");
        }
        assert_eq!(
            infer(&[
                "sh".into(),
                "-c".into(),
                "cd x && cargo build && echo done".into()
            ]),
            JobKind::Build
        );
        assert_eq!(
            infer(&["bash".into(), "-c".into(), "cargo test; cargo bench".into()]),
            JobKind::Bench,
            "the heaviest command in the line decides"
        );
        assert_eq!(
            infer(&["sh".into(), "-c".into(), "echo hi".into()]),
            JobKind::Other
        );
    }

    #[test]
    fn a_linked_worktree_reads_the_main_checkouts_rules_not_its_own() {
        let t = tempfile::tempdir().unwrap();
        let main = t.path().join("main");
        let wt = t.path().join("wt");
        fs::create_dir_all(main.join(".git/worktrees/wt")).unwrap();
        fs::create_dir_all(wt.join("sub")).unwrap();
        fs::write(main.join(".git/worktrees/wt/commondir"), "../..\n").unwrap();
        fs::write(
            wt.join(".git"),
            format!("gitdir: {}\n", main.join(".git/worktrees/wt").display()),
        )
        .unwrap();
        fs::write(
            main.join("crew.toml"),
            "[usher.kinds]\nbench = [\"./run-bench\"]\n",
        )
        .unwrap();
        // The branch tries to turn its bench into a harmless job.
        fs::write(
            wt.join("crew.toml"),
            "[usher.kinds]\nother = [\"./run-bench\"]\n",
        )
        .unwrap();
        let rules = ProjectRules::discover(&wt.join("sub")).unwrap();
        assert_eq!(infer_with(&words("./run-bench"), &rules), JobKind::Bench);
        // A plain checkout reads its own file.
        let plain = ProjectRules::discover(&main).unwrap();
        assert_eq!(infer_with(&words("./run-bench"), &plain), JobKind::Bench);
        fs::remove_file(main.join("crew.toml")).unwrap();
        let none = ProjectRules::discover(&wt).unwrap();
        assert_eq!(
            infer_with(&words("./run-bench"), &none),
            JobKind::Other,
            "the branch's own file is not used"
        );
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
