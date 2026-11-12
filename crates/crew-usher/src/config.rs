use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crew_depot_config::Depot;
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
    #[error("{path}: {problem}")]
    Invalid { path: String, problem: String },
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

fn watched_paths_for(home: Option<&Path>) -> Vec<String> {
    let mut paths = vec!["/".to_string()];
    let device = |p: &Path| fs::metadata(p).map(|m| m.dev()).ok();
    if let Some(home) = home
        && let (Some(h), Some(root)) = (device(home), device(Path::new("/")))
        && h != root
    {
        paths.push(home.display().to_string());
    }
    paths
}

/// `/`, plus `$HOME` when it lives on a different filesystem.
fn default_watched_paths() -> Vec<String> {
    watched_paths_for(std::env::var_os("HOME").map(PathBuf::from).as_deref())
}

fn tmp_root_for(runtime_dir: Option<&Path>, temp_dir: &Path) -> PathBuf {
    match runtime_dir {
        Some(d) if !d.as_os_str().is_empty() => d.join("crew/tmp"),
        _ => temp_dir.join("crew"),
    }
}

/// `$XDG_RUNTIME_DIR/crew/tmp`, else `<system temp dir>/crew`.
fn default_tmp_root() -> String {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    tmp_root_for(runtime.as_deref(), &std::env::temp_dir())
        .display()
        .to_string()
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
    /// A manual `crew owner away` ends after this long, so a forgotten one cannot admit benches forever.
    pub manual_away_max_secs: u64,
    /// Runs shorter than this, and runs that failed, teach the usher nothing about a command's size.
    pub learn_min_wall_secs: u64,
    /// Open client connections beyond this are turned away; every queued client and waiter holds one.
    pub max_connections: usize,
    pub kinds: Kinds,
    pub adopt: Adopt,
    /// From depot.toml, not usher.toml: the main program loads it beside this file.
    #[serde(skip)]
    pub depot: Depot,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            min_free_mem_gib: 16,
            max_games: 4,
            cpu_oversub: 1.5,
            watched_paths: default_watched_paths(),
            min_free_disk_gib: BTreeMap::from([("/".to_string(), 20)]),
            owner_present_weights: OwnerWeights::default(),
            tmp_root: default_tmp_root(),
            idle_timeout_secs: 120,
            starve_after_secs: 600,
            manual_away_max_secs: 8 * 3600,
            learn_min_wall_secs: 5,
            max_connections: 512,
            kinds: Kinds::default(),
            adopt: Adopt::default(),
            depot: Depot::default(),
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
            Ok(text) => {
                let mut cfg: Self = toml::from_str(&text).map_err(|source| ConfigError::Parse {
                    path: shown.clone(),
                    source,
                })?;
                // A threshold the file never wrote is the built-in `/`; it must not break a config that watches other paths.
                let wrote_thresholds = text
                    .parse::<toml::Table>()
                    .is_ok_and(|t| t.contains_key("min_free_disk_gib"));
                if !wrote_thresholds {
                    let watched = cfg.watched_paths.clone();
                    cfg.min_free_disk_gib.retain(|p, _| watched.contains(p));
                }
                cfg.validate().map_err(|problem| ConfigError::Invalid {
                    path: shown,
                    problem,
                })?;
                Ok(cfg)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(ConfigError::Read {
                path: shown,
                source,
            }),
        }
    }

    /// A disk threshold for a path nobody measures would never be enforced.
    pub fn validate(&self) -> Result<(), String> {
        match self
            .min_free_disk_gib
            .keys()
            .find(|p| !self.watched_paths.contains(p))
        {
            Some(p) => Err(format!(
                "min_free_disk_gib names {p}, which is not in watched_paths {:?}; add it there or remove the threshold",
                self.watched_paths
            )),
            None => Ok(()),
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

    fn device_of(p: &Path) -> u64 {
        fs::metadata(p).unwrap().dev()
    }

    #[test]
    fn defaults_match_the_spec() {
        let c = Config::default();
        assert_eq!((c.min_free_mem_gib, c.max_games), (16, 4));
        assert_eq!(c.min_free_disk("/"), Some(20 * GIB));
        assert_eq!(c.min_free_disk_gib.len(), 1);
        assert_eq!(c.watched_paths[0], "/");
        assert_eq!(c.kind(JobKind::Game).mem_cap, Some(6 * GIB));
        assert_eq!(c.kind(JobKind::Lsp).cpu_weight, 20);
        assert!(c.kind(JobKind::Bench).exclusive);
        assert_eq!(c.kind(JobKind::Build).expected_cores, 4.0);
    }

    #[test]
    fn partial_toml_overrides_only_what_it_names() {
        let c: Config = toml::from_str("max_games = 2\n[kinds.build]\nexpected_cores = 8.0\n[min_free_disk_gib]\n\"/work\" = 10\n").unwrap();
        assert_eq!(c.max_games, 2);
        assert_eq!(c.kind(JobKind::Build).expected_cores, 8.0);
        assert_eq!(c.kind(JobKind::Build).cpu_weight, 100);
        assert_eq!(c.min_free_disk("/work"), Some(10 * GIB));
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
    fn defaults_name_no_machine_specific_path() {
        let c = Config::default();
        assert!(c.watched_paths.iter().all(|p| !p.starts_with("/srv-data")));
        assert!(!c.tmp_root.contains("/srv-data"));
        assert!(Path::new(&c.tmp_root).is_absolute());
    }

    #[test]
    fn tmp_root_prefers_the_runtime_dir() {
        let tmp = Path::new("/tmp");
        assert_eq!(
            tmp_root_for(Some(Path::new("/run/user/7")), tmp),
            Path::new("/run/user/7/crew/tmp")
        );
        assert_eq!(tmp_root_for(None, tmp), Path::new("/tmp/crew"));
        assert_eq!(
            tmp_root_for(Some(Path::new("")), tmp),
            Path::new("/tmp/crew")
        );
    }

    #[test]
    fn home_is_watched_only_when_it_is_on_another_filesystem() {
        assert_eq!(watched_paths_for(None), ["/"]);
        assert_eq!(watched_paths_for(Some(Path::new("/"))), ["/"]);
        assert_eq!(
            watched_paths_for(Some(Path::new("/nonexistent-home"))),
            ["/"]
        );
        // /dev/shm is a tmpfs on every systemd machine.
        let shm = Path::new("/dev/shm");
        if shm.is_dir() && device_of(shm) != device_of(Path::new("/")) {
            assert_eq!(watched_paths_for(Some(shm)), ["/", "/dev/shm"]);
        }
    }

    #[test]
    fn a_pre_neutral_usher_toml_still_loads() {
        let text = "watched_paths = [\"/data\", \"/\"]\ntmp_root = \"/data/tmp/crew\"\n[min_free_disk_gib]\n\"/data\" = 50\n\"/\" = 20\n";
        let c: Config = toml::from_str(text).unwrap();
        assert_eq!(c.watched_paths, ["/data", "/"]);
        assert_eq!(c.tmp_root, "/data/tmp/crew");
        assert_eq!(c.min_free_disk("/data"), Some(50 * GIB));
    }

    /// The first ```toml fence after `<!-- test:NAME -->` in the README.
    fn readme_block(name: &str) -> String {
        let readme = include_str!("../../../README.md");
        let after = readme
            .split(&format!("<!-- test:{name} -->"))
            .nth(1)
            .unwrap();
        let body = after.split("```toml\n").nth(1).unwrap();
        body.split("```").next().unwrap().to_string()
    }

    #[test]
    fn the_readme_defaults_are_the_defaults() {
        let documented: Config = toml::from_str(&readme_block("usher-defaults")).unwrap();
        let actual = Config::default();
        let same_apart_from_machine_paths = Config {
            watched_paths: actual.watched_paths.clone(),
            tmp_root: actual.tmp_root.clone(),
            ..documented
        };
        assert_eq!(same_apart_from_machine_paths, actual);
    }

    #[test]
    fn the_readme_override_example_loads() {
        let c: Config = toml::from_str(&readme_block("usher-overrides")).unwrap();
        assert_eq!(c.kind(JobKind::Build).expected_cores, 8.0);
        assert_eq!(c.min_free_disk("/data"), Some(100 * GIB));
    }

    #[test]
    fn a_disk_threshold_needs_its_path_in_watched_paths() {
        let c: Config =
            toml::from_str("watched_paths = [\"/\"]\n[min_free_disk_gib]\n\"/data\" = 10\n")
                .unwrap();
        let why = c.validate().unwrap_err();
        assert!(
            why.contains("/data") && why.contains("watched_paths"),
            "{why}"
        );
        let ok: Config = toml::from_str(
            "watched_paths = [\"/\", \"/data\"]\n[min_free_disk_gib]\n\"/data\" = 10\n",
        )
        .unwrap();
        assert!(ok.validate().is_ok());
        assert!(Config::default().validate().is_ok());
    }

    #[test]
    fn loading_keeps_a_config_that_watches_other_paths_working_but_rejects_a_written_threshold_nobody_measures()
     {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usher.toml");
        fs::write(&path, "watched_paths = [\"/data\"]\n").unwrap();
        let c = Config::load(&path).unwrap();
        assert!(
            c.min_free_disk_gib.is_empty(),
            "the built-in / threshold names an unwatched path"
        );
        fs::write(
            &path,
            "watched_paths = [\"/data\"]\n[min_free_disk_gib]\n\"/\" = 20\n",
        )
        .unwrap();
        assert!(matches!(
            Config::load(&path),
            Err(ConfigError::Invalid { .. })
        ));
    }

    #[test]
    fn unknown_keys_are_errors() {
        assert!(toml::from_str::<Config>("max_gamez = 2\n").is_err());
    }
}
