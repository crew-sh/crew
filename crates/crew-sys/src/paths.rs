use std::env;
use std::path::PathBuf;

fn uid() -> u32 {
    unsafe { libc::getuid() }
}

fn home() -> PathBuf {
    env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from)
}

fn xdg(var: &str, fallback: impl FnOnce() -> PathBuf) -> PathBuf {
    match env::var_os(var) {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => fallback(),
    }
}

/// `CREW_SOCKET` overrides the default for tests and side-by-side daemons.
pub fn socket_path() -> PathBuf {
    if let Some(p) = env::var_os("CREW_SOCKET") {
        return PathBuf::from(p);
    }
    xdg("XDG_RUNTIME_DIR", || {
        PathBuf::from(format!("/run/user/{}", uid()))
    })
    .join("crew/usher.sock")
}

pub fn state_dir() -> PathBuf {
    xdg("XDG_STATE_HOME", || home().join(".local/state")).join("crew")
}

pub fn config_dir() -> PathBuf {
    xdg("XDG_CONFIG_HOME", || home().join(".config")).join("crew")
}
