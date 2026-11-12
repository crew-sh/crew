//! The `[depot]` table, read from `~/.config/crew/depot.toml` and the project's `crew.toml` (the project wins).
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;

const DEFAULT_RESERVE_CPUS: u32 = 4;
const DEFAULT_RESERVE_MEMORY_GIB: u64 = 8;
const DEFAULT_STORE_BUDGET_GIB: u64 = 60;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("reading {path}: {source}")]
    Read { path: String, source: io::Error },
    #[error("parsing [depot] in {path}: {message}")]
    Parse { path: String, message: String },
}

/// Whether builds go through mbx, the dependency cache.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MbxMode {
    /// Use mbx when it is installed, else plain cargo.
    #[default]
    Auto,
    Off,
    /// A missing mbx is an error.
    Required,
}

/// Cargo's `lto` profile setting.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Lto {
    Off,
    Thin,
    Fat,
}

impl Lto {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Thin => "thin",
            Self::Fat => "fat",
        }
    }
}

/// The profile fields `crew cargo` may override for agent builds; every one is opt-in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProfileOverride {
    pub lto: Option<Lto>,
    #[serde(rename = "codegen-units")]
    pub codegen_units: Option<u32>,
}

/// One `[depot]` table as written: an absent key means "not set here", so two tables can be layered.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DepotTable {
    /// A rustup toolchain name (`1.99.1`, `stable`) that every build of the project uses.
    pub toolchain: Option<String>,
    pub mbx: Option<MbxMode>,
    /// The mbx program; default: `PATH`, then cargo's bin directory.
    pub mbx_path: Option<PathBuf>,
    /// The usher holds part of mbx's pool while the owner is present, and `crew run` wraps games and harnesses in `mbx reserve`.
    pub mbx_reserve: Option<bool>,
    pub reserve_cpus: Option<u32>,
    pub reserve_memory_gib: Option<u64>,
    /// Where depot keeps its records; default: `<cache dir>/crew`.
    pub store: Option<PathBuf>,
    /// Size budget for the store's snapshots. Not enforced yet: nothing is stored there before milestone 2.
    pub store_budget_gib: Option<u64>,
    pub profiles: BTreeMap<String, ProfileOverride>,
}

/// Only `[depot]` is ours; every other table in the file belongs to other tools.
#[derive(Deserialize)]
struct File {
    depot: Option<DepotTable>,
}

fn valid_profile_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

impl DepotTable {
    /// The `[depot]` table of a TOML text; no table means no settings.
    pub fn parse(text: &str, shown_path: &str) -> Result<Self, ConfigError> {
        let bad = |message: String| ConfigError::Parse {
            path: shown_path.to_string(),
            message,
        };
        let file: File = toml::from_str(text).map_err(|e| bad(e.to_string()))?;
        let table = file.depot.unwrap_or_default();
        if let Some(name) = table.profiles.keys().find(|n| !valid_profile_name(n)) {
            return Err(bad(format!("profiles.{name:?} is not a profile name")));
        }
        Ok(table)
    }

    fn read(path: &Path) -> Result<Option<Self>, ConfigError> {
        let shown = path.display().to_string();
        match fs::read_to_string(path) {
            Ok(text) => Self::parse(&text, &shown).map(Some),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(ConfigError::Read {
                path: shown,
                source,
            }),
        }
    }

    /// `depot.toml` in the crew config directory.
    pub fn user() -> Result<Self, ConfigError> {
        Self::user_in(&crew_sys::paths::config_dir())
    }

    pub fn user_in(config_dir: &Path) -> Result<Self, ConfigError> {
        Ok(Self::read(&config_dir.join("depot.toml"))?.unwrap_or_default())
    }

    /// The nearest `crew.toml` at or above `dir`; none means no settings.
    pub fn project(dir: &Path) -> Result<Self, ConfigError> {
        let found = dir
            .ancestors()
            .map(|d| d.join("crew.toml"))
            .find(|p| p.is_file());
        match found {
            Some(path) => Ok(Self::read(&path)?.unwrap_or_default()),
            None => Ok(Self::default()),
        }
    }

    /// The keys only the user's own `depot.toml` may set (`mbx_path`, `store`) that this table sets.
    /// A project's `crew.toml` comes with a checked-out branch, and must not choose which program runs or where records go.
    pub fn user_only_keys_set(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.mbx_path.is_some() {
            out.push("mbx_path");
        }
        if self.store.is_some() {
            out.push("store");
        }
        out
    }

    /// `self` is the base; every key `project` sets replaces it (except the user-only ones), and profiles merge field by field.
    pub fn layered(mut self, project: Self) -> Self {
        macro_rules! take {
            ($($f:ident),*) => {$(if project.$f.is_some() { self.$f = project.$f; })*};
        }
        take!(
            toolchain,
            mbx,
            mbx_reserve,
            reserve_cpus,
            reserve_memory_gib,
            store_budget_gib
        );
        for (name, over) in project.profiles {
            let mine = self.profiles.entry(name).or_default();
            mine.lto = over.lto.or(mine.lto);
            mine.codegen_units = over.codegen_units.or(mine.codegen_units);
        }
        self
    }

    pub fn resolve(self) -> Depot {
        Depot {
            toolchain: self.toolchain.filter(|t| !t.is_empty()),
            mbx: self.mbx.unwrap_or_default(),
            mbx_path: self.mbx_path,
            mbx_reserve: self.mbx_reserve.unwrap_or(false),
            reserve_cpus: self.reserve_cpus.unwrap_or(DEFAULT_RESERVE_CPUS),
            reserve_memory_gib: self
                .reserve_memory_gib
                .unwrap_or(DEFAULT_RESERVE_MEMORY_GIB),
            store: self.store.unwrap_or_else(crew_sys::paths::cache_dir),
            store_budget_gib: self.store_budget_gib.unwrap_or(DEFAULT_STORE_BUDGET_GIB),
            profiles: self.profiles,
        }
    }
}

/// The settings with every default filled in.
#[derive(Clone, Debug, PartialEq)]
pub struct Depot {
    pub toolchain: Option<String>,
    pub mbx: MbxMode,
    pub mbx_path: Option<PathBuf>,
    pub mbx_reserve: bool,
    pub reserve_cpus: u32,
    pub reserve_memory_gib: u64,
    pub store: PathBuf,
    pub store_budget_gib: u64,
    pub profiles: BTreeMap<String, ProfileOverride>,
}

impl Default for Depot {
    fn default() -> Self {
        DepotTable::default().resolve()
    }
}

/// `CARGO_PROFILE_FAST_LTO` for profile `fast` and field `LTO`; dashes become underscores, as cargo reads them.
fn profile_var(profile: &str, field: &str) -> String {
    format!(
        "CARGO_PROFILE_{}_{field}",
        profile.to_ascii_uppercase().replace('-', "_")
    )
}

/// The first executable `name` in `dirs`.
fn find_in(name: &str, dirs: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
    dirs.into_iter().map(|d| d.join(name)).find(|p| is_exec(p))
}

fn is_exec(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

impl Depot {
    /// Environment variables that apply the profile overrides; the caller keeps any it already has.
    pub fn profile_env(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for (name, p) in &self.profiles {
            if let Some(lto) = p.lto {
                out.push((profile_var(name, "LTO"), lto.as_str().to_string()));
            }
            if let Some(n) = p.codegen_units {
                out.push((profile_var(name, "CODEGEN_UNITS"), n.to_string()));
            }
        }
        out
    }

    /// The mbx program: `mbx_path`, else `path_var`'s directories, else cargo's bin directory. None when `mbx = "off"`.
    pub fn find_mbx_in(&self, path_var: Option<&OsStr>, cargo_bin: &Path) -> Option<PathBuf> {
        if self.mbx == MbxMode::Off {
            return None;
        }
        if let Some(p) = &self.mbx_path {
            return is_exec(p).then(|| p.clone());
        }
        let on_path = path_var.into_iter().flat_map(std::env::split_paths);
        find_in("mbx", on_path.chain([cargo_bin.to_path_buf()]))
    }

    pub fn find_mbx(&self) -> Option<PathBuf> {
        self.find_mbx_in(
            std::env::var_os("PATH").as_deref(),
            &crew_sys::paths::cargo_bin_dir(),
        )
    }
}

#[cfg(test)]
mod tests;
