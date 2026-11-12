use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::Path;

use crew_proto::JobKind;
use serde::Deserialize;

pub const GIB: u64 = 1 << 30;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("reading {path}: {source}")]
    Read { path: String, source: io::Error },
    #[error("parsing {path}: {source}")]
    Parse {
        path: String,
        source: toml::de::Error,
    },
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OwnerWeights {
    pub cpu: u32,
    pub io: u32,
}

impl Default for OwnerWeights {
    fn default() -> Self {
        Self { cpu: 20, io: 20 }
    }
}

/// Agent adoption: sessions are always observed; `move` and `freeze` only ever log what they would do.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Adopt {
    pub enabled: bool,
    #[serde(rename = "move")]
    pub move_: bool,
    pub freeze: bool,
    /// Agent names (CREW_AGENT) and pids that adoption must never act on.
    pub protect: Vec<String>,
    /// Cpu pressure (psi some avg10) above which the busiest actionable agent would be frozen.
    pub freeze_psi_cpu: f64,
}

impl Default for Adopt {
    fn default() -> Self {
        Self {
            enabled: true,
            move_: false,
            freeze: false,
            protect: Vec::new(),
            freeze_psi_cpu: 40.0,
        }
    }
}

/// Per-kind overrides of the built-in defaults.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct KindOverride {
    pub mem_cap_gib: Option<u64>,
    pub expected_mem_gib: Option<u64>,
    pub expected_cores: Option<f64>,
    pub cpu_weight: Option<u32>,
    pub io_weight: Option<u32>,
    pub exclusive: Option<bool>,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Kinds {
    pub build: KindOverride,
    pub test: KindOverride,
    pub harness: KindOverride,
    pub game: KindOverride,
    pub bench: KindOverride,
    pub lsp: KindOverride,
    pub other: KindOverride,
}

#[derive(Clone, Debug, PartialEq)]
pub struct KindSpec {
    pub mem_cap: Option<u64>,
    pub expected_mem: u64,
    pub expected_cores: f64,
    pub cpu_weight: u32,
    pub io_weight: u32,
    pub exclusive: bool,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub min_free_mem_gib: u64,
    pub max_games: u32,
    pub cpu_oversub: f64,
    pub watched_paths: Vec<String>,
    pub min_free_disk_gib: BTreeMap<String, u64>,
    pub owner_present_weights: OwnerWeights,
    pub tmp_root: String,
    pub idle_timeout_secs: u64,
    pub starve_after_secs: u64,
    pub kinds: Kinds,
    pub adopt: Adopt,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            min_free_mem_gib: 16,
            max_games: 4,
            cpu_oversub: 1.5,
            watched_paths: vec!["/projects".into(), "/".into()],
            min_free_disk_gib: BTreeMap::from([
                ("/projects".to_string(), 50),
                ("/".to_string(), 20),
            ]),
            owner_present_weights: OwnerWeights::default(),
            tmp_root: "/projects/.hz-tmp/crew".into(),
            idle_timeout_secs: 120,
            starve_after_secs: 600,
            kinds: Kinds::default(),
            adopt: Adopt::default(),
        }
    }
}

fn builtin(kind: JobKind) -> KindSpec {
    let base = KindSpec {
        mem_cap: None,
        expected_mem: 2 * GIB,
        expected_cores: 1.0,
        cpu_weight: 100,
        io_weight: 100,
        exclusive: false,
    };
    match kind {
        JobKind::Build => KindSpec {
            expected_cores: 4.0,
            ..base
        },
        JobKind::Game => KindSpec {
            mem_cap: Some(6 * GIB),
            expected_mem: 6 * GIB,
            ..base
        },
        JobKind::Bench => KindSpec {
            exclusive: true,
            ..base
        },
        JobKind::Lsp => KindSpec {
            cpu_weight: 20,
            ..base
        },
        JobKind::Test | JobKind::Harness | JobKind::Other | JobKind::Unspecified => base,
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let shown = path.display().to_string();
        match fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).map_err(|source| ConfigError::Parse {
                path: shown,
                source,
            }),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(ConfigError::Read {
                path: shown,
                source,
            }),
        }
    }

    pub fn kind(&self, kind: JobKind) -> KindSpec {
        let o = match kind {
            JobKind::Build => &self.kinds.build,
            JobKind::Test => &self.kinds.test,
            JobKind::Harness => &self.kinds.harness,
            JobKind::Game => &self.kinds.game,
            JobKind::Bench => &self.kinds.bench,
            JobKind::Lsp => &self.kinds.lsp,
            JobKind::Other | JobKind::Unspecified => &self.kinds.other,
        };
        let b = builtin(kind);
        KindSpec {
            mem_cap: o.mem_cap_gib.map(|g| g * GIB).or(b.mem_cap),
            expected_mem: o.expected_mem_gib.map_or(b.expected_mem, |g| g * GIB),
            expected_cores: o.expected_cores.unwrap_or(b.expected_cores),
            cpu_weight: o.cpu_weight.unwrap_or(b.cpu_weight),
            io_weight: o.io_weight.unwrap_or(b.io_weight),
            exclusive: o.exclusive.unwrap_or(b.exclusive),
        }
    }

    pub fn min_free_mem(&self) -> u64 {
        self.min_free_mem_gib * GIB
    }

    pub fn min_free_disk(&self, path: &str) -> Option<u64> {
        self.min_free_disk_gib.get(path).map(|g| g * GIB)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_spec() {
        let c = Config::default();
        assert_eq!((c.min_free_mem_gib, c.max_games), (16, 4));
        assert_eq!(c.min_free_disk("/projects"), Some(50 * GIB));
        assert_eq!(c.kind(JobKind::Game).mem_cap, Some(6 * GIB));
        assert_eq!(c.kind(JobKind::Lsp).cpu_weight, 20);
        assert!(c.kind(JobKind::Bench).exclusive);
        assert_eq!(c.kind(JobKind::Build).expected_cores, 4.0);
    }

    #[test]
    fn partial_toml_overrides_only_what_it_names() {
        let c: Config = toml::from_str("max_games = 2\n[kinds.build]\nexpected_cores = 8.0\n[min_free_disk_gib]\n\"/projects\" = 10\n").unwrap();
        assert_eq!(c.max_games, 2);
        assert_eq!(c.kind(JobKind::Build).expected_cores, 8.0);
        assert_eq!(c.kind(JobKind::Build).cpu_weight, 100);
        assert_eq!(c.min_free_disk("/projects"), Some(10 * GIB));
        assert_eq!(c.min_free_disk("/"), None);
    }

    #[test]
    fn adoption_acts_on_nothing_by_default() {
        let c = Config::default();
        assert!(c.adopt.enabled && !c.adopt.move_ && !c.adopt.freeze);
        let c: Config = toml::from_str("[adopt]\nmove = true\nprotect = [\"lead\"]\n").unwrap();
        assert!(c.adopt.move_ && !c.adopt.freeze && c.adopt.protect == ["lead"]);
    }

    #[test]
    fn unknown_keys_are_errors() {
        assert!(toml::from_str::<Config>("max_gamez = 2\n").is_err());
    }
}
