use std::fs;
use std::io;
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum MachineError {
    #[error("reading {path}: {source}")]
    Read { path: String, source: io::Error },
    #[error("cannot parse {what}")]
    Parse { what: &'static str },
    #[error("statvfs {path}: {source}")]
    Statvfs { path: String, source: io::Error },
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Psi {
    pub cpu_some_avg10: f64,
    pub mem_some_avg10: f64,
    pub io_some_avg10: f64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiskFree {
    pub path: String,
    pub free_bytes: u64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    pub mem_total: u64,
    pub mem_available: u64,
    pub cpu_count: u32,
    pub load1: f64,
    pub psi: Psi,
    pub disk: Vec<DiskFree>,
    /// Busy CPU time of the whole machine since boot.
    pub cpu_busy_usec: u64,
}

fn read(path: &str) -> Result<String, MachineError> {
    fs::read_to_string(path).map_err(|source| MachineError::Read {
        path: path.to_string(),
        source,
    })
}

pub fn parse_meminfo(text: &str) -> Result<(u64, u64), MachineError> {
    let field = |name: &str| -> Option<u64> {
        let line = text.lines().find(|l| l.starts_with(name))?;
        let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
        Some(kib * 1024)
    };
    match (field("MemTotal:"), field("MemAvailable:")) {
        (Some(t), Some(a)) => Ok((t, a)),
        _ => Err(MachineError::Parse {
            what: "/proc/meminfo",
        }),
    }
}

pub fn parse_loadavg(text: &str) -> Result<f64, MachineError> {
    text.split_whitespace()
        .next()
        .and_then(|v| v.parse().ok())
        .ok_or(MachineError::Parse {
            what: "/proc/loadavg",
        })
}

/// A missing "some" line (PSI off) reads as zero.
pub fn parse_psi_some_avg10(text: &str) -> f64 {
    text.lines()
        .find(|l| l.starts_with("some "))
        .and_then(|l| l.split_whitespace().find_map(|f| f.strip_prefix("avg10=")))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.0)
}

pub fn parse_stat_busy_usec(text: &str, ticks_per_sec: u64) -> Result<u64, MachineError> {
    let line = text
        .lines()
        .find(|l| l.starts_with("cpu "))
        .ok_or(MachineError::Parse { what: "/proc/stat" })?;
    let v: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|f| f.parse().ok())
        .collect();
    // user nice system idle iowait irq softirq steal; guest time is already inside user.
    if v.len() < 8 {
        return Err(MachineError::Parse { what: "/proc/stat" });
    }
    let busy = v[0] + v[1] + v[2] + v[5] + v[6] + v[7];
    Ok(busy * 1_000_000 / ticks_per_sec)
}

pub fn cpu_count() -> u32 {
    let n = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };
    u32::try_from(n).unwrap_or(1).max(1)
}

fn ticks_per_sec() -> u64 {
    u64::try_from(unsafe { libc::sysconf(libc::_SC_CLK_TCK) })
        .unwrap_or(100)
        .max(1)
}

pub fn disk_free(path: &str) -> Result<u64, MachineError> {
    let c = std::ffi::CString::new(path).map_err(|_| MachineError::Parse {
        what: "path with NUL",
    })?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return Err(MachineError::Statvfs {
            path: path.to_string(),
            source: io::Error::last_os_error(),
        });
    }
    Ok(st.f_bavail as u64 * st.f_frsize as u64)
}

pub fn snapshot(watched: &[String]) -> Result<Snapshot, MachineError> {
    let (mem_total, mem_available) = parse_meminfo(&read("/proc/meminfo")?)?;
    let psi = |name: &str| {
        fs::read_to_string(Path::new("/proc/pressure").join(name))
            .map_or(0.0, |t| parse_psi_some_avg10(&t))
    };
    let mut disk = Vec::new();
    for path in watched {
        disk.push(DiskFree {
            path: path.clone(),
            free_bytes: disk_free(path)?,
        });
    }
    Ok(Snapshot {
        mem_total,
        mem_available,
        cpu_count: cpu_count(),
        load1: parse_loadavg(&read("/proc/loadavg")?)?,
        psi: Psi {
            cpu_some_avg10: psi("cpu"),
            mem_some_avg10: psi("memory"),
            io_some_avg10: psi("io"),
        },
        disk,
        cpu_busy_usec: parse_stat_busy_usec(&read("/proc/stat")?, ticks_per_sec())?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meminfo_gives_bytes() {
        let t = "MemTotal:       32768000 kB\nMemFree: 1 kB\nMemAvailable:   16384000 kB\n";
        assert_eq!(
            parse_meminfo(t).unwrap(),
            (32_768_000 * 1024, 16_384_000 * 1024)
        );
        assert!(parse_meminfo("MemTotal: 1 kB\n").is_err());
    }

    #[test]
    fn loadavg_first_field() {
        assert_eq!(parse_loadavg("3.25 2.00 1.10 4/900 1234\n").unwrap(), 3.25);
        assert!(parse_loadavg("").is_err());
    }

    #[test]
    fn psi_reads_some_avg10() {
        let t = "some avg10=12.50 avg60=3.00 avg300=1.00 total=99\nfull avg10=9.00 avg60=0 avg300=0 total=1\n";
        assert_eq!(parse_psi_some_avg10(t), 12.5);
        assert_eq!(parse_psi_some_avg10(""), 0.0);
    }

    #[test]
    fn stat_busy_excludes_idle_and_iowait() {
        let t = "cpu  100 10 50 1000 500 5 5 5 0 0\ncpu0 1 1 1 1 1 1 1 1 0 0\n";
        assert_eq!(
            parse_stat_busy_usec(t, 100).unwrap(),
            (100 + 10 + 50 + 5 + 5 + 5) * 10_000
        );
    }

    #[test]
    fn live_snapshot_is_sane() {
        let s = snapshot(&["/".to_string()]).unwrap();
        assert!(s.mem_total >= s.mem_available && s.mem_available > 0);
        assert!(s.cpu_count >= 1);
        assert!(s.disk[0].free_bytes > 0);
        assert!(snapshot(&["/definitely/not/here".to_string()]).is_err());
    }
}
