use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScopeLimits {
    pub mem_bytes: Option<u64>,
    pub cpu_weight: u32,
    pub io_weight: u32,
    pub cpus: Option<u32>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub mem_peak: u64,
    pub mem_current: u64,
    /// Anonymous, shared and kernel memory: what the job holds, without the page cache it only read or wrote.
    /// One sample from `stats`; `observe` keeps the highest.
    pub mem_work: u64,
    pub cpu_usec: u64,
    pub io_read: u64,
    pub io_write: u64,
}

impl Stats {
    /// Takes `newer` as the current reading while remembering the highest memory seen so far.
    pub fn observe(&mut self, newer: Self) {
        *self = Self {
            mem_peak: newer.mem_peak.max(self.mem_peak),
            mem_work: newer.mem_work.max(self.mem_work),
            ..newer
        };
    }

    /// The peak worth reserving for: the working set, or the cgroup peak when `memory.stat` was unreadable.
    pub fn peak_worth_learning(&self) -> u64 {
        if self.mem_work > 0 {
            self.mem_work
        } else {
            self.mem_peak
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KillSignal {
    Term,
    Kill,
}

fn uid() -> u32 {
    unsafe { libc::getuid() }
}

pub fn cgroup_dir(name: &str) -> PathBuf {
    let uid = uid();
    PathBuf::from(format!(
        "/sys/fs/cgroup/user.slice/user-{uid}.slice/user@{uid}.service/app.slice/{name}.scope"
    ))
}

fn property_args(limits: &ScopeLimits) -> Vec<String> {
    let mut args = Vec::new();
    let mut prop = |p: String| {
        args.push("-p".to_string());
        args.push(p);
    };
    if let Some(mem) = limits.mem_bytes {
        prop(format!("MemoryMax={mem}"));
    }
    if limits.cpu_weight > 0 {
        prop(format!("CPUWeight={}", limits.cpu_weight));
    }
    if limits.io_weight > 0 {
        prop(format!("IOWeight={}", limits.io_weight));
    }
    if let Some(cpus) = limits.cpus {
        prop(format!("CPUQuota={}%", u64::from(cpus) * 100));
    }
    args
}

fn command(
    argv: &[String],
    cwd: &Path,
    env: &BTreeMap<String, String>,
    limits: &ScopeLimits,
    name: &str,
) -> Command {
    let mut cmd = Command::new("systemd-run");
    cmd.args(["--user", "--scope", "--quiet"])
        .arg(format!("--unit={name}"))
        .args(property_args(limits))
        .arg("--")
        .args(argv)
        .current_dir(cwd)
        .env_clear()
        .envs(env);
    cmd
}

/// Runs `argv` in a transient user scope; systemd-run execs the command itself, so the child is the command.
pub fn spawn(
    argv: &[String],
    cwd: &Path,
    env: &BTreeMap<String, String>,
    limits: &ScopeLimits,
    name: &str,
) -> io::Result<Child> {
    let mut cmd = command(argv, cwd, env, limits, name);
    cmd.stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    cmd.spawn()
}

// Runs the command, then writes its exit status (128+signal when killed) to the file named by $1 through a rename.
const STATUS_SCRIPT: &str =
    r#"s=$1; shift; "$@"; c=$?; printf '%s\n' "$c" >"$s.tmp" && mv -f "$s.tmp" "$s"; exit "$c""#;

/// `argv` behind a wrapper that leaves its exit status in `status`, for a job whose parent may not live to wait for it.
pub fn status_wrapped(argv: &[String], status: &Path) -> Vec<String> {
    let mut out = vec![
        "sh".to_string(),
        "-c".to_string(),
        STATUS_SCRIPT.to_string(),
        "crew-job".to_string(),
        status.display().to_string(),
    ];
    out.extend_from_slice(argv);
    out
}

/// The exit status a `status_wrapped` job left behind; `None` while there is none.
pub fn read_status(status: &Path) -> Option<i32> {
    fs::read_to_string(status).ok()?.trim().parse().ok()
}

/// Like `spawn`, with no stdin and stdout and stderr appended to `log`.
pub fn spawn_logged(
    argv: &[String],
    cwd: &Path,
    env: &BTreeMap<String, String>,
    limits: &ScopeLimits,
    name: &str,
    log: fs::File,
) -> io::Result<Child> {
    let mut cmd = command(argv, cwd, env, limits, name);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log));
    cmd.spawn()
}

/// Whether the scope's cgroup still exists, that is, whether anything in it still runs.
pub fn alive(name: &str) -> bool {
    cgroup_dir(name).is_dir()
}

pub fn kill(name: &str, signal: KillSignal) {
    let sig = match signal {
        KillSignal::Term => "SIGTERM",
        KillSignal::Kill => "SIGKILL",
    };
    let _ = Command::new("systemctl")
        .args(["--user", "kill", "--signal", sig, &format!("{name}.scope")])
        .output();
}

/// Asks systemd to stop the scope and everything in it (SIGTERM, then SIGKILL after its stop timeout); does not wait.
pub fn stop(name: &str) {
    let _ = Command::new("systemctl")
        .args(["--user", "stop", "--no-block", &format!("{name}.scope")])
        .output();
}

pub fn set_weights(name: &str, cpu_weight: u32, io_weight: u32) {
    let _ = Command::new("systemctl")
        .args([
            "--user",
            "set-property",
            "--runtime",
            &format!("{name}.scope"),
        ])
        .arg(format!("CPUWeight={cpu_weight}"))
        .arg(format!("IOWeight={io_weight}"))
        .output();
}

pub fn parse_cpu_usage_usec(text: &str) -> u64 {
    text.lines()
        .find_map(|l| l.strip_prefix("usage_usec "))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0)
}

/// Sums rbytes and wbytes over every device line.
pub fn parse_io_stat(text: &str) -> (u64, u64) {
    let mut read = 0;
    let mut write = 0;
    for field in text.split_whitespace() {
        if let Some(v) = field.strip_prefix("rbytes=") {
            read += v.parse::<u64>().unwrap_or(0);
        } else if let Some(v) = field.strip_prefix("wbytes=") {
            write += v.parse::<u64>().unwrap_or(0);
        }
    }
    (read, write)
}

/// `anon + shmem + kernel` of a `memory.stat`; older kernels without `kernel` get its parts.
pub fn parse_working_set(text: &str) -> u64 {
    let field = |name: &str| -> Option<u64> {
        text.lines()
            .find_map(|l| l.strip_prefix(name)?.strip_prefix(' '))
            .and_then(|v| v.trim().parse().ok())
    };
    let kernel = field("kernel").unwrap_or_else(|| {
        ["slab", "kernel_stack", "pagetables", "percpu"]
            .iter()
            .filter_map(|k| field(k))
            .sum()
    });
    field("anon").unwrap_or(0) + field("shmem").unwrap_or(0) + kernel
}

fn read_u64(path: &Path) -> u64 {
    fs::read_to_string(path)
        .ok()
        .and_then(|t| t.trim().parse().ok())
        .unwrap_or(0)
}

/// `None` once the scope's cgroup is gone.
pub fn stats(name: &str) -> Option<Stats> {
    stats_at(&cgroup_dir(name))
}

/// Reads the accounting files of any cgroup directory.
pub fn stats_at(dir: &Path) -> Option<Stats> {
    let cpu = fs::read_to_string(dir.join("cpu.stat")).ok()?;
    let (io_read, io_write) =
        fs::read_to_string(dir.join("io.stat")).map_or((0, 0), |t| parse_io_stat(&t));
    Some(Stats {
        mem_peak: read_u64(&dir.join("memory.peak")),
        mem_current: read_u64(&dir.join("memory.current")),
        mem_work: fs::read_to_string(dir.join("memory.stat")).map_or(0, |t| parse_working_set(&t)),
        cpu_usec: parse_cpu_usage_usec(&cpu),
        io_read,
        io_write,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn properties_follow_the_limits() {
        let l = ScopeLimits {
            mem_bytes: Some(1 << 30),
            cpu_weight: 100,
            io_weight: 0,
            cpus: Some(4),
        };
        assert_eq!(
            property_args(&l),
            [
                "-p",
                "MemoryMax=1073741824",
                "-p",
                "CPUWeight=100",
                "-p",
                "CPUQuota=400%"
            ]
        );
        assert!(property_args(&ScopeLimits::default()).is_empty());
    }

    #[test]
    fn cgroup_files_parse() {
        assert_eq!(parse_cpu_usage_usec("usage_usec 4200\nuser_usec 1\n"), 4200);
        assert_eq!(parse_cpu_usage_usec(""), 0);
        let io = "259:0 rbytes=100 wbytes=50 rios=1 wios=1\n8:0 rbytes=1 wbytes=2 rios=0 wios=0\n";
        assert_eq!(parse_io_stat(io), (101, 52));
    }

    #[test]
    fn the_working_set_leaves_out_page_cache() {
        let stat = "anon 1000\nfile 90000\nkernel 50\nshmem 200\nfile_mapped 5\n";
        assert_eq!(parse_working_set(stat), 1250);
        // Without a `kernel` line its parts are summed.
        let old = "anon 1000\nfile 90000\nslab 7\nkernel_stack 3\npagetables 2\npercpu 1\n";
        assert_eq!(parse_working_set(old), 1013);
        assert_eq!(parse_working_set(""), 0);
    }

    #[test]
    fn a_wrapped_command_leaves_its_exit_status_behind() {
        let dir = tempfile::tempdir().unwrap();
        for (script, want) in [("exit 0", 0), ("exit 7", 7), ("kill -9 $$", 137)] {
            let status = dir.path().join("status");
            let argv = ["sh".to_string(), "-c".to_string(), script.to_string()];
            let out = Command::new("env")
                .args(status_wrapped(&argv, &status))
                .status()
                .unwrap();
            assert_eq!(read_status(&status), Some(want), "{script}");
            assert_eq!(out.code().or(Some(137)), Some(want), "{script}");
            std::fs::remove_file(&status).unwrap();
        }
        assert_eq!(read_status(&dir.path().join("none")), None);
    }

    #[test]
    fn cgroup_path_is_under_the_user_manager() {
        let p = cgroup_dir("crew-x");
        assert!(p.ends_with(
            "user@".to_string() + &uid().to_string() + ".service/app.slice/crew-x.scope"
        ));
    }
}
