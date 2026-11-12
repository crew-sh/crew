use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::path::Path;

use crew_proto::{JobKind, JobRequest, Limits};

const DISPLAY_VARS: [&str; 3] = ["WAYLAND_DISPLAY", "WAYLAND_SOCKET", "DISPLAY"];

/// 128 random bits as a version-4 UUID.
pub fn new_id() -> String {
    let mut b = [0u8; 16];
    if File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .is_err()
    {
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        b = (t ^ u128::from(std::process::id())).to_le_bytes();
    }
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    )
}

pub use crew_sys::paths::project_of;

pub struct Inputs<'a> {
    pub kind: JobKind,
    pub argv: &'a [String],
    pub cwd: &'a Path,
    pub env: &'a BTreeMap<String, String>,
    pub mem_bytes: Option<u64>,
    pub cpus: Option<u32>,
    pub display: bool,
    pub exclusive: bool,
}

pub fn build(i: &Inputs<'_>) -> JobRequest {
    let declared = (i.mem_bytes.is_some() || i.cpus.is_some()).then(|| Limits {
        mem_bytes: i.mem_bytes,
        cpus: i.cpus,
        ..Limits::default()
    });
    JobRequest {
        id: new_id(),
        kind: i.kind as i32,
        argv: i.argv.to_vec(),
        cwd: i.cwd.display().to_string(),
        project: project_of(i.cwd),
        agent: i.env.get("CREW_AGENT").cloned().unwrap_or_default(),
        declared,
        needs_display: i.display,
        exclusive: i.exclusive,
        desktop: i.env.get("CREW_DESKTOP").is_some_and(|v| v == "1"),
        display_env: DISPLAY_VARS
            .iter()
            .filter_map(|k| i.env.get(*k).map(|v| ((*k).to_string(), v.clone())))
            .collect(),
        ..JobRequest::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_look_like_v4_uuids_and_differ() {
        let a = new_id();
        assert_eq!(a.len(), 36);
        assert_eq!(a.as_bytes()[14], b'4');
        assert_ne!(a, new_id());
    }

    #[test]
    fn project_is_the_git_toplevel_basename() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("myrepo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("a/b")).unwrap();
        assert_eq!(project_of(&repo.join("a/b")), "myrepo");
        let plain = dir.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        assert_eq!(project_of(&plain), "plain");
    }

    #[test]
    fn request_carries_env_derived_fields() {
        let env: BTreeMap<String, String> = [
            ("CREW_AGENT", "a1"),
            ("CREW_DESKTOP", "1"),
            ("DISPLAY", ":0"),
            ("HOME", "/h"),
        ]
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
        let argv = vec!["true".to_string()];
        let r = build(&Inputs {
            kind: JobKind::Other,
            argv: &argv,
            cwd: Path::new("/tmp"),
            env: &env,
            mem_bytes: Some(1),
            cpus: None,
            display: true,
            exclusive: true,
        });
        assert_eq!(
            (r.agent.as_str(), r.desktop, r.needs_display, r.exclusive),
            ("a1", true, true, true)
        );
        assert_eq!(r.display_env.len(), 1);
        assert_eq!(r.declared.unwrap().mem_bytes, Some(1));
    }
}
