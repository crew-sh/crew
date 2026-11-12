//! Per-project build-cache numbers: mbx's build report folded into a record per repository, plus mbx's own lifetime totals.
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use crew_proto::depot::{BuildCounts, ProjectStats};
use crew_proto::prost::Message;
use serde::{Deserialize, Serialize};

const STATS_VERSION: u32 = 1;
/// Compiler probes mbx lists as bypasses that never mean a lost cache entry.
const ROUTINE_BYPASSES: [&str; 2] = ["compiler-query", "standard-input"];

/// The fields of mbx's `MBX_STATS_REPORT` file (version 5) that depot reads; the rest is ignored.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct MbxReport {
    pub session_duration_ns: u64,
    pub hits: u64,
    pub misses: u64,
    pub unconsulted: u64,
    pub incremental_compilations: u64,
    pub estimated_compiler_duration_avoided_ns: u64,
    pub bypasses: BTreeMap<String, u64>,
}

impl MbxReport {
    pub fn counts(&self) -> BuildCounts {
        let bypasses = self
            .bypasses
            .iter()
            .filter(|(k, _)| !ROUTINE_BYPASSES.contains(&k.as_str()) && !k.starts_with("cc-"))
            .map(|(_, n)| n)
            .sum();
        BuildCounts {
            hits: self.hits,
            misses: self.misses,
            unconsulted: self.unconsulted,
            incremental: self.incremental_compilations,
            bypasses,
            compiler_ns_avoided: self.estimated_compiler_duration_avoided_ns,
            wall_ns: self.session_duration_ns,
        }
    }
}

/// mbx's machine-wide totals from `mbx stats --json`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Lifetime {
    pub hits: u64,
    pub misses: u64,
    pub unconsulted: u64,
    pub bypasses: u64,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct StatsJson {
    savings: SavingsJson,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct SavingsJson {
    compilations: Lifetime,
}

pub fn parse_lifetime(json: &str) -> Result<Lifetime, serde_json::Error> {
    Ok(serde_json::from_str::<StatsJson>(json)?
        .savings
        .compilations)
}

pub fn add(into: &mut BuildCounts, c: &BuildCounts) {
    into.hits += c.hits;
    into.misses += c.misses;
    into.unconsulted += c.unconsulted;
    into.incremental += c.incremental;
    into.bypasses += c.bypasses;
    into.compiler_ns_avoided += c.compiler_ns_avoided;
    into.wall_ns += c.wall_ns;
}

/// FNV-1a: a stable name for a path, not a security hash.
fn fnv(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// `<store>/stats/<repo name>-<hash of its root>.pb`: one record per repository, worktrees included.
pub fn record_path(store: &Path, root: &Path) -> PathBuf {
    let name = root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project");
    let name: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    store.join("stats").join(format!(
        "{name}-{:016x}.pb",
        fnv(&root.display().to_string())
    ))
}

pub fn load(path: &Path) -> Option<ProjectStats> {
    let bytes = fs::read(path).ok()?;
    ProjectStats::decode(bytes.as_slice())
        .ok()
        .filter(|s| s.version == STATS_VERSION)
}

struct Lock {
    _file: File,
}

impl Lock {
    fn exclusive(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        // SAFETY: flock on a descriptor we own.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { _file: file })
    }
}

pub struct Build<'a> {
    pub root: &'a Path,
    pub command: &'a str,
    pub counts: BuildCounts,
    pub at_secs: u64,
}

/// Adds one build to the repository's record. An unreadable old record starts over.
pub fn record(store: &Path, b: &Build<'_>) -> io::Result<()> {
    let path = record_path(store, b.root);
    fs::create_dir_all(path.parent().expect("record path has a parent"))?;
    let _lock = Lock::exclusive(&path.with_extension("lock"))?;
    let mut s = load(&path).unwrap_or_default();
    s.version = STATS_VERSION;
    s.project = crew_sys::paths::project_of(b.root);
    s.root = b.root.display().to_string();
    s.builds += 1;
    s.last_at_secs = b.at_secs;
    s.last_command = b.command.to_string();
    add(s.total.get_or_insert_with(Default::default), &b.counts);
    s.last = Some(b.counts);
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, s.encode_to_vec())?;
    fs::rename(tmp, path)
}

fn secs(ns: u64) -> String {
    format!("{:.1}s", ns as f64 / 1e9)
}

fn counts_line(c: &BuildCounts) -> String {
    format!(
        "hits {}  misses {}  not looked up {}  incremental {}  bypassed {}  compiler time avoided {}  mbx time {}",
        c.hits,
        c.misses,
        c.unconsulted,
        c.incremental,
        c.bypasses,
        secs(c.compiler_ns_avoided),
        secs(c.wall_ns)
    )
}

pub fn render(s: Option<&ProjectStats>, life: Option<&Lifetime>, now_secs: u64) -> String {
    let mut out = String::new();
    match s {
        None => out.push_str("no build of this project has gone through `crew cargo` yet\n"),
        Some(s) => {
            out.push_str(&format!("project   {} ({})\n", s.project, s.root));
            out.push_str(&format!("builds    {} through crew cargo\n", s.builds));
            let ago = now_secs.saturating_sub(s.last_at_secs);
            out.push_str(&format!(
                "last      `cargo {}`, {ago}s ago\n",
                s.last_command
            ));
            out.push_str(&format!(
                "          {}\n",
                counts_line(&s.last.unwrap_or_default())
            ));
            out.push_str(&format!(
                "total     {}\n",
                counts_line(&s.total.unwrap_or_default())
            ));
        }
    }
    if let Some(l) = life {
        out.push_str(&format!(
            "machine   all projects, since mbx began counting: hits {}  misses {}  not looked up {}  bypassed {}\n",
            l.hits, l.misses, l.unconsulted, l.bypasses
        ));
    }
    out
}

#[derive(Serialize)]
pub struct CountsJson {
    pub hits: u64,
    pub misses: u64,
    pub unconsulted: u64,
    pub incremental: u64,
    pub bypasses: u64,
    pub compiler_ns_avoided: u64,
    pub wall_ns: u64,
}

impl From<&BuildCounts> for CountsJson {
    fn from(c: &BuildCounts) -> Self {
        Self {
            hits: c.hits,
            misses: c.misses,
            unconsulted: c.unconsulted,
            incremental: c.incremental,
            bypasses: c.bypasses,
            compiler_ns_avoided: c.compiler_ns_avoided,
            wall_ns: c.wall_ns,
        }
    }
}

#[derive(Serialize)]
pub struct StatsView {
    pub project: Option<String>,
    pub root: Option<String>,
    pub builds: u64,
    pub last_command: Option<String>,
    pub last_at_secs: Option<u64>,
    pub last: Option<CountsJson>,
    pub total: Option<CountsJson>,
    pub machine: Option<Lifetime>,
}

pub fn json(s: Option<&ProjectStats>, life: Option<Lifetime>) -> StatsView {
    StatsView {
        project: s.map(|s| s.project.clone()),
        root: s.map(|s| s.root.clone()),
        builds: s.map_or(0, |s| s.builds),
        last_command: s.map(|s| s.last_command.clone()),
        last_at_secs: s.map(|s| s.last_at_secs),
        last: s.and_then(|s| s.last.as_ref()).map(Into::into),
        total: s.and_then(|s| s.total.as_ref()).map(Into::into),
        machine: life,
    }
}

#[cfg(test)]
mod tests;
