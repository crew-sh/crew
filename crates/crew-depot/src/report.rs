//! `crew build report --shared`: for each dependency unit of this project, whether another project on the machine compiles the same thing, or the first reason it does not.
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// A value cargo prints as a number, a string, a flag or a small table, depending on the field.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum Setting {
    Flag(bool),
    Number(i64),
    Text(String),
    Table(BTreeMap<String, Setting>),
}

impl std::fmt::Display for Setting {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Flag(b) => write!(f, "{b}"),
            Self::Number(n) => write!(f, "{n}"),
            Self::Text(t) => f.write_str(t),
            Self::Table(t) => {
                let parts: Vec<_> = t.iter().map(|(k, v)| format!("{k}: {v}")).collect();
                write!(f, "{{{}}}", parts.join(", "))
            }
        }
    }
}

/// The compilation settings of a unit that reach rustc's arguments (`incremental` never does for dependencies).
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Profile {
    pub opt_level: Option<Setting>,
    pub lto: Option<Setting>,
    pub codegen_backend: Option<Setting>,
    pub codegen_units: Option<Setting>,
    pub debuginfo: Option<Setting>,
    pub split_debuginfo: Option<Setting>,
    pub debug_assertions: Option<Setting>,
    pub overflow_checks: Option<Setting>,
    pub rpath: Option<Setting>,
    pub panic: Option<Setting>,
    pub strip: Option<Setting>,
}

impl Profile {
    fn fields(&self) -> [(&'static str, &Option<Setting>); 11] {
        [
            ("opt-level", &self.opt_level),
            ("lto", &self.lto),
            ("codegen-backend", &self.codegen_backend),
            ("codegen-units", &self.codegen_units),
            ("debuginfo", &self.debuginfo),
            ("split-debuginfo", &self.split_debuginfo),
            ("debug-assertions", &self.debug_assertions),
            ("overflow-checks", &self.overflow_checks),
            ("rpath", &self.rpath),
            ("panic", &self.panic),
            ("strip", &self.strip),
        ]
    }

    /// The first field that differs, in a fixed order.
    fn first_difference(&self, other: &Self) -> Option<(&'static str, String, String)> {
        let show =
            |s: &Option<Setting>| s.as_ref().map_or("unset".to_string(), ToString::to_string);
        self.fields()
            .into_iter()
            .zip(other.fields())
            .find(|((_, a), (_, b))| a != b)
            .map(|((name, a), (_, b))| (name, show(a), show(b)))
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Target {
    pub kind: Vec<String>,
    pub crate_types: Vec<String>,
    pub name: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Dep {
    pub index: usize,
    #[serde(default)]
    pub extern_crate_name: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Unit {
    pub pkg_id: String,
    pub target: Target,
    #[serde(default)]
    pub profile: Profile,
    #[serde(default)]
    pub platform: Option<String>,
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub features: Vec<String>,
    #[serde(default)]
    pub dependencies: Vec<Dep>,
}

#[derive(Debug, Deserialize)]
pub struct UnitGraph {
    pub units: Vec<Unit>,
}

/// `(name, version, is a workspace or path unit)` from a package id such as `registry+https://host/index#serde@1.0.0`.
pub fn package(pkg_id: &str) -> (String, String, bool) {
    let local = pkg_id.starts_with("path+");
    let Some((source, tail)) = pkg_id.rsplit_once('#') else {
        return (pkg_id.to_string(), String::new(), local);
    };
    match tail.split_once('@') {
        Some((n, v)) => (n.to_string(), v.to_string(), local),
        None => {
            let name = source.rsplit('/').next().unwrap_or(source);
            (name.to_string(), tail.to_string(), local)
        }
    }
}

impl Unit {
    pub fn name(&self) -> String {
        package(&self.pkg_id).0
    }

    pub fn version(&self) -> String {
        package(&self.pkg_id).1
    }

    pub fn is_dependency(&self) -> bool {
        !package(&self.pkg_id).2
    }

    /// What kind of compilation this is, apart from which package: `lib check`, `build-script run`.
    pub fn role(&self) -> String {
        format!("{} {}", self.target.kind.join("+"), self.mode)
    }

    fn slot(&self) -> (String, &[String], &[String], &str, &str, &Option<String>) {
        (
            self.name(),
            &self.target.kind,
            &self.target.crate_types,
            &self.target.name,
            &self.mode,
            &self.platform,
        )
    }

    fn sorted_features(&self) -> BTreeSet<&str> {
        self.features.iter().map(String::as_str).collect()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "reason")]
pub enum Verdict {
    Shared,
    /// Another compiler: nothing is shared until both projects use the same one.
    Rustc {
        here: String,
        there: String,
    },
    /// The other project does not use the package.
    NotUsed,
    /// The other project's lock file holds other versions of it.
    LockVersion {
        there: Vec<String>,
    },
    /// Same version, but not built in this mode or for this target there.
    OtherMode,
    Features {
        only_here: Vec<String>,
        only_there: Vec<String>,
    },
    ProfileField {
        field: String,
        here: String,
        there: String,
    },
    /// A dependency of this unit is not shared; its own line says why.
    Dependency {
        name: String,
    },
    /// Every dependency is shared, but the other unit links a different set.
    DependencySet,
}

impl Verdict {
    fn text(&self) -> String {
        match self {
            Self::Shared => "shared".into(),
            Self::Rustc { here, there } => format!("rustc differs ({here} here, {there} there)"),
            Self::NotUsed => "not used by the other project".into(),
            Self::LockVersion { there } => {
                format!("lock version differs (there: {})", there.join(", "))
            }
            Self::OtherMode => "same version, but not built in this mode there".into(),
            Self::Features {
                only_here,
                only_there,
            } => format!(
                "features differ (only here: [{}], only there: [{}])",
                only_here.join(", "),
                only_there.join(", ")
            ),
            Self::ProfileField { field, here, there } => {
                format!("profile {field} differs ({here} here, {there} there)")
            }
            Self::Dependency { name } => format!("dependency {name} is not shared"),
            Self::DependencySet => "links a different set of dependencies".into(),
        }
    }

    fn tag(&self) -> &'static str {
        match self {
            Self::Shared => "shared",
            Self::Rustc { .. } => "rustc",
            Self::NotUsed => "not used there",
            Self::LockVersion { .. } => "lock version",
            Self::OtherMode => "other mode",
            Self::Features { .. } => "features",
            Self::ProfileField { .. } => "profile",
            Self::Dependency { .. } => "dependency",
            Self::DependencySet => "dependency set",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UnitReport {
    pub package: String,
    pub version: String,
    pub role: String,
    #[serde(flatten)]
    pub verdict: Verdict,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub rustc_here: String,
    pub rustc_there: String,
    pub units: Vec<UnitReport>,
    pub summary: BTreeMap<String, usize>,
}

struct Matcher<'a> {
    here: &'a UnitGraph,
    there: &'a UnitGraph,
    memo: Vec<Option<Option<usize>>>,
}

impl Matcher<'_> {
    /// The unit over there that is the same compilation as `i`, dependencies included.
    fn matched(&mut self, i: usize) -> Option<usize> {
        if let Some(m) = self.memo[i] {
            return m;
        }
        // A graph has no cycles; a placeholder keeps a malformed one from recursing forever.
        self.memo[i] = Some(None);
        let u = &self.here.units[i];
        let deps: Vec<Option<usize>> = u
            .dependencies
            .iter()
            .map(|d| self.matched(d.index))
            .collect();
        let found = self.there.units.iter().enumerate().find_map(|(j, c)| {
            let same = c.slot() == u.slot()
                && c.version() == u.version()
                && c.sorted_features() == u.sorted_features()
                && c.profile == u.profile;
            let deps_ok = deps.len() == c.dependencies.len()
                && deps
                    .iter()
                    .all(|m| m.is_some_and(|k| c.dependencies.iter().any(|d| d.index == k)));
            (same && deps_ok).then_some(j)
        });
        self.memo[i] = Some(found);
        found
    }

    fn why_not(&mut self, i: usize) -> Verdict {
        let u = &self.here.units[i];
        let name = u.name();
        let same_package: Vec<&Unit> = self
            .there
            .units
            .iter()
            .filter(|c| c.name() == name)
            .collect();
        if same_package.is_empty() {
            return Verdict::NotUsed;
        }
        let same_version: Vec<&Unit> = same_package
            .iter()
            .copied()
            .filter(|c| c.version() == u.version())
            .collect();
        if same_version.is_empty() {
            let mut there: Vec<String> = same_package.iter().map(|c| c.version()).collect();
            there.sort();
            there.dedup();
            return Verdict::LockVersion { there };
        }
        let in_slot: Vec<&Unit> = same_version
            .iter()
            .copied()
            .filter(|c| c.slot() == u.slot())
            .collect();
        let Some(near) = in_slot.first() else {
            return Verdict::OtherMode;
        };
        // The closest candidate by features, then profile.
        let best = in_slot
            .iter()
            .find(|c| c.sorted_features() == u.sorted_features())
            .unwrap_or(near);
        let (mine, theirs) = (u.sorted_features(), best.sorted_features());
        if mine != theirs {
            return Verdict::Features {
                only_here: mine.difference(&theirs).map(|s| (*s).to_string()).collect(),
                only_there: theirs.difference(&mine).map(|s| (*s).to_string()).collect(),
            };
        }
        if let Some((field, here, there)) = u.profile.first_difference(&best.profile) {
            return Verdict::ProfileField {
                field: field.into(),
                here,
                there,
            };
        }
        let deps: Vec<usize> = u.dependencies.iter().map(|d| d.index).collect();
        for d in deps {
            if self.matched(d).is_none() {
                return Verdict::Dependency {
                    name: self.here.units[d].name(),
                };
            }
        }
        Verdict::DependencySet
    }
}

/// Compares every dependency unit of `here` with `there`. Differing compilers decide every unit at once.
pub fn compare(here: &UnitGraph, there: &UnitGraph, rustc_here: &str, rustc_there: &str) -> Report {
    let mut m = Matcher {
        here,
        there,
        memo: vec![None; here.units.len()],
    };
    let mut units = Vec::new();
    for (i, u) in here
        .units
        .iter()
        .enumerate()
        .filter(|(_, u)| u.is_dependency())
    {
        let verdict = if rustc_here != rustc_there {
            Verdict::Rustc {
                here: rustc_here.into(),
                there: rustc_there.into(),
            }
        } else if m.matched(i).is_some() {
            Verdict::Shared
        } else {
            m.why_not(i)
        };
        units.push(UnitReport {
            package: u.name(),
            version: u.version(),
            role: u.role(),
            verdict,
        });
    }
    let mut summary = BTreeMap::new();
    for u in &units {
        *summary.entry(u.verdict.tag().to_string()).or_insert(0) += 1;
    }
    Report {
        rustc_here: rustc_here.into(),
        rustc_there: rustc_there.into(),
        units,
        summary,
    }
}

pub fn render(r: &Report, summary_only: bool) -> String {
    let mut out = format!(
        "rustc here  {}\nrustc there {}\n\n",
        r.rustc_here, r.rustc_there
    );
    let shared = r.summary.get("shared").copied().unwrap_or(0);
    out.push_str(&format!(
        "{shared} of {} dependency units are shared\n",
        r.units.len()
    ));
    for (tag, n) in r.summary.iter().filter(|(t, _)| t.as_str() != "shared") {
        out.push_str(&format!("  {n:>4}  {tag}\n"));
    }
    if !summary_only {
        out.push('\n');
        for u in &r.units {
            out.push_str(&format!(
                "{} {} ({}): {}\n",
                u.package,
                u.version,
                u.role,
                u.verdict.text()
            ));
        }
    }
    out
}

/// The compiler's identity: its commit when it prints one, else its release.
pub fn parse_rustc_version(text: &str) -> Option<String> {
    let field = |k: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(k))
            .map(|v| v.trim().to_string())
    };
    let release = field("release:")?;
    let host = field("host:").unwrap_or_default();
    let commit = field("commit-hash:").filter(|c| c != "unknown");
    Some(match commit {
        Some(c) => format!("{release} ({}) {host}", &c[..c.len().min(9)]),
        None => format!("{release} {host}"),
    })
}

/// What `cargo build|check --unit-graph` needs to run in `dir`; cargo only plans, no compiler runs.
pub struct GraphRun<'a> {
    pub dir: &'a Path,
    pub check: bool,
    pub profile: Option<&'a str>,
    pub toolchain: Option<&'a str>,
    pub env: &'a [(String, String)],
}

fn command(program: &str, g: &GraphRun<'_>) -> Command {
    let mut c = Command::new(program);
    c.current_dir(g.dir).env_remove("RUSTC_WRAPPER");
    if let Some(t) = g.toolchain {
        c.env("RUSTUP_TOOLCHAIN", t);
    }
    c.envs(g.env.iter().map(|(k, v)| (k, v)));
    c
}

pub fn rustc_version(g: &GraphRun<'_>) -> Result<String> {
    let out = command("rustc", g)
        .arg("-vV")
        .output()
        .context("running rustc -vV")?;
    if !out.status.success() {
        bail!("rustc -vV failed in {}", g.dir.display());
    }
    parse_rustc_version(&String::from_utf8_lossy(&out.stdout)).context("unreadable `rustc -vV`")
}

/// `RUSTC_BOOTSTRAP` is set on this one planning command: `--unit-graph` is unstable, and no compiler runs under it.
pub fn unit_graph(g: &GraphRun<'_>) -> Result<UnitGraph> {
    let mut c = command("cargo", g);
    c.env("RUSTC_BOOTSTRAP", "1")
        .arg(if g.check { "check" } else { "build" })
        .args([
            "--unit-graph",
            "-Zunstable-options",
            "--locked",
            "--workspace",
        ]);
    if let Some(p) = g.profile {
        c.args(["--profile", p]);
    }
    let out = c.output().context("running cargo")?;
    if !out.status.success() {
        bail!(
            "cargo --unit-graph failed in {}: {}",
            g.dir.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    serde_json::from_slice(&out.stdout).context("reading cargo's unit graph")
}

#[cfg(test)]
mod tests;
