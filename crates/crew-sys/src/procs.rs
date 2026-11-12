use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProcStat {
    pub pid: u32,
    pub comm: String,
    pub ppid: u32,
    pub tty_nr: i64,
    /// utime + stime + cutime + cstime: the process plus the children it has waited for.
    pub cpu_ticks: u64,
    pub rss_pages: u64,
    pub start_ticks: u64,
}

/// Parses `/proc/<pid>/stat`; the command name sits in parentheses and may itself contain spaces and parentheses.
pub fn parse_stat(text: &str) -> Option<ProcStat> {
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    let pid = text[..open].trim().parse().ok()?;
    let rest: Vec<&str> = text.get(close + 1..)?.split_whitespace().collect();
    // rest[0] is field 3 (state), so field n is rest[n - 3].
    let num = |field: usize| rest.get(field - 3)?.parse::<i64>().ok();
    let ticks = |field: usize| u64::try_from(num(field)?).ok();
    Some(ProcStat {
        pid,
        comm: text[open + 1..close].to_string(),
        ppid: u32::try_from(num(4)?).ok()?,
        tty_nr: num(7)?,
        cpu_ticks: ticks(14)? + ticks(15)? + ticks(16)? + ticks(17)?,
        rss_pages: ticks(24).unwrap_or(0),
        start_ticks: ticks(22)?,
    })
}

/// `read_bytes` and `write_bytes` of `/proc/<pid>/io`: what reached the block layer.
pub fn parse_io(text: &str) -> (u64, u64) {
    let field = |name: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(name))
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0)
    };
    (field("read_bytes:"), field("write_bytes:"))
}

/// The cgroup v2 path of `/proc/<pid>/cgroup`.
pub fn parse_cgroup_path(text: &str) -> Option<String> {
    text.lines()
        .find_map(|l| l.strip_prefix("0::"))
        .map(|p| p.trim().to_string())
}

pub fn parse_env_var(environ: &[u8], name: &str) -> Option<String> {
    environ.split(|b| *b == 0).find_map(|entry| {
        let entry = std::str::from_utf8(entry).ok()?;
        entry
            .strip_prefix(name)?
            .strip_prefix('=')
            .map(str::to_string)
    })
}

pub fn clock_ticks_per_sec() -> u64 {
    u64::try_from(unsafe { libc::sysconf(libc::_SC_CLK_TCK) })
        .unwrap_or(100)
        .max(1)
}

pub fn page_size() -> u64 {
    u64::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) })
        .unwrap_or(4096)
        .max(1)
}

pub fn list_pids(root: &Path) -> Vec<u32> {
    fs::read_dir(root)
        .map(|d| {
            d.flatten()
                .filter_map(|e| e.file_name().to_str()?.parse().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Every readable process, by pid; processes that exit mid-scan are skipped.
pub fn read_all_stats(root: &Path) -> BTreeMap<u32, ProcStat> {
    list_pids(root)
        .into_iter()
        .filter_map(|pid| {
            let text = fs::read_to_string(root.join(pid.to_string()).join("stat")).ok()?;
            Some((pid, parse_stat(&text)?))
        })
        .collect()
}

fn proc_file(root: &Path, pid: u32, name: &str) -> PathBuf {
    root.join(pid.to_string()).join(name)
}

pub fn read_io(root: &Path, pid: u32) -> Option<(u64, u64)> {
    fs::read_to_string(proc_file(root, pid, "io"))
        .ok()
        .map(|t| parse_io(&t))
}

pub fn read_cgroup_path(root: &Path, pid: u32) -> Option<String> {
    parse_cgroup_path(&fs::read_to_string(proc_file(root, pid, "cgroup")).ok()?)
}

pub fn read_cwd(root: &Path, pid: u32) -> Option<PathBuf> {
    fs::read_link(proc_file(root, pid, "cwd")).ok()
}

pub fn read_env_var(root: &Path, pid: u32, name: &str) -> Option<String> {
    parse_env_var(&fs::read(proc_file(root, pid, "environ")).ok()?, name)
}

/// Pids listed directly in the cgroup directory, `None` when it is gone.
pub fn cgroup_pids(dir: &Path) -> Option<Vec<u32>> {
    let text = fs::read_to_string(dir.join("cgroup.procs")).ok()?;
    Some(text.lines().filter_map(|l| l.trim().parse().ok()).collect())
}

pub fn has_child_cgroups(dir: &Path) -> bool {
    fs::read_dir(dir).is_ok_and(|d| d.flatten().any(|e| e.file_type().is_ok_and(|t| t.is_dir())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_survives_awkward_command_names() {
        let line = "42 (we ird) name) S 7 42 42 34816 42 4194560 1 2 3 4 100 50 10 5 20 0 1 0 999 1000 77 18446744073709551615";
        let s = parse_stat(line).unwrap();
        assert_eq!(s.comm, "we ird) name");
        assert_eq!((s.pid, s.ppid, s.tty_nr), (42, 7, 34816));
        assert_eq!(s.cpu_ticks, 100 + 50 + 10 + 5);
        assert_eq!((s.start_ticks, s.rss_pages), (999, 77));
        assert_eq!(parse_stat("garbage"), None);
        assert_eq!(parse_stat("1 (x) S"), None);
    }

    #[test]
    fn io_and_cgroup_and_env_files_parse() {
        let io =
            "rchar: 9\nwchar: 9\nread_bytes: 4096\nwrite_bytes: 8192\ncancelled_write_bytes: 1\n";
        assert_eq!(parse_io(io), (4096, 8192));
        assert_eq!(parse_io(""), (0, 0));
        assert_eq!(
            parse_cgroup_path("1:net_cls:/\n0::/user.slice/x.scope\n").as_deref(),
            Some("/user.slice/x.scope")
        );
        assert_eq!(parse_cgroup_path("1:name=x:/a\n"), None);
        let env = b"A=1\0CREW_AGENT=t1\0CREW_AGENT_X=no\0";
        assert_eq!(parse_env_var(env, "CREW_AGENT").as_deref(), Some("t1"));
        assert_eq!(parse_env_var(env, "B"), None);
    }

    #[test]
    fn fake_proc_tree_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("7");
        fs::create_dir(&p).unwrap();
        fs::write(
            p.join("stat"),
            "7 (claude) S 1 7 7 0 -1 0 0 0 0 0 10 20 0 0 20 0 1 0 5 0 3 0",
        )
        .unwrap();
        fs::write(p.join("io"), "read_bytes: 1\nwrite_bytes: 2\n").unwrap();
        fs::write(p.join("environ"), b"CREW_AGENT=zed\0").unwrap();
        fs::write(p.join("cgroup"), "0::/a/b.scope\n").unwrap();
        std::os::unix::fs::symlink("/tmp", p.join("cwd")).unwrap();
        fs::create_dir(dir.path().join("self")).unwrap();
        let all = read_all_stats(dir.path());
        assert_eq!(all.keys().copied().collect::<Vec<_>>(), [7]);
        assert_eq!(all[&7].comm, "claude");
        assert_eq!(read_io(dir.path(), 7), Some((1, 2)));
        assert_eq!(
            read_env_var(dir.path(), 7, "CREW_AGENT").as_deref(),
            Some("zed")
        );
        assert_eq!(
            read_cgroup_path(dir.path(), 7).as_deref(),
            Some("/a/b.scope")
        );
        assert_eq!(read_cwd(dir.path(), 7), Some(PathBuf::from("/tmp")));
        assert_eq!(read_io(dir.path(), 8), None);
    }

    #[test]
    fn live_proc_lists_this_process() {
        let me = std::process::id();
        let all = read_all_stats(Path::new("/proc"));
        assert!(all.contains_key(&me));
        assert!(read_cgroup_path(Path::new("/proc"), me).is_some());
    }

    #[test]
    fn cgroup_dir_helpers() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("cgroup.procs"), "5\n9\n").unwrap();
        assert_eq!(cgroup_pids(dir.path()), Some(vec![5, 9]));
        assert!(!has_child_cgroups(dir.path()));
        fs::create_dir(dir.path().join("child")).unwrap();
        assert!(has_child_cgroups(dir.path()));
        assert_eq!(cgroup_pids(&dir.path().join("none")), None);
    }
}
