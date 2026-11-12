use std::fmt::Write;

use crew_proto::{JobKind, JobView, MachineView, OwnerSource, TopReply, WaitingOn};

const GIB: f64 = (1u64 << 30) as f64;

fn gib(bytes: u64) -> String {
    format!("{:.1}", bytes as f64 / GIB)
}

fn duration(ms: u64) -> String {
    let s = ms / 1000;
    if s >= 3600 {
        format!("{}h{:02}m", s / 3600, s % 3600 / 60)
    } else if s >= 60 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        format!("{s}s")
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max - 1).collect::<String>())
    }
}

fn kind_name(v: &JobView) -> &'static str {
    JobKind::try_from(v.kind)
        .unwrap_or(JobKind::Unspecified)
        .name()
}

fn machine_lines(m: &MachineView, out: &mut String) {
    let owner = if m.owner_present { "present" } else { "away" };
    let source = match OwnerSource::try_from(m.owner_source).unwrap_or(OwnerSource::Unspecified) {
        OwnerSource::Manual => "manual",
        OwnerSource::WaylandIdle => "wayland idle",
        OwnerSource::Default => "assumed, no idle event yet",
        OwnerSource::Auto | OwnerSource::Unspecified => "?",
    };
    let _ = writeln!(
        out,
        "memory   {} of {} GiB available",
        gib(m.mem_available),
        gib(m.mem_total)
    );
    let _ = writeln!(
        out,
        "cpu      {} cores, load {:.2}, unmanaged {:.0}%",
        m.cpu_count, m.load1, m.unmanaged_cpu_pct
    );
    let _ = writeln!(
        out,
        "psi      cpu {:.1}  mem {:.1}  io {:.1}",
        m.psi_cpu_some_avg10, m.psi_mem_some_avg10, m.psi_io_some_avg10
    );
    for d in &m.disk {
        let _ = writeln!(
            out,
            "disk     {} {} GiB free (min {})",
            d.path,
            gib(d.free_bytes),
            gib(d.min_free_bytes)
        );
    }
    let _ = writeln!(out, "owner    {owner} ({source})");
}

fn running_row(v: &JobView) -> String {
    let actual = if v.age_ms > 0 {
        v.cpu_usec as f64 / (v.age_ms as f64 * 1000.0)
    } else {
        0.0
    };
    format!(
        "{:<8} {:<10} {:<12} {:>7} {:>4.1}/{:<4.1} {:>6}/{:<6} {:>8}  {}",
        kind_name(v),
        truncate(&v.agent, 10),
        truncate(&v.project, 12),
        duration(v.age_ms),
        actual,
        f64::from(v.expected_cores_milli) / 1000.0,
        gib(v.mem_bytes),
        gib(v.expected_mem_bytes),
        format!("{:.0}M", v.io_bytes as f64 / (1u64 << 20) as f64),
        truncate(&v.command, 60),
    )
}

fn queued_row(v: &JobView) -> String {
    let waiting = WaitingOn::try_from(v.waiting_on).unwrap_or(WaitingOn::Unspecified);
    format!(
        "{:<8} {:<10} {:<12} {:>7} {:<14} {:>6}  {}",
        kind_name(v),
        truncate(&v.agent, 10),
        truncate(&v.project, 12),
        duration(v.age_ms),
        waiting.name(),
        gib(v.expected_mem_bytes),
        truncate(&v.command, 60),
    )
}

pub fn render(t: &TopReply) -> String {
    let mut out = String::new();
    if let Some(m) = &t.machine {
        machine_lines(m, &mut out);
    }
    let _ = writeln!(out, "\nRUNNING ({})", t.running.len());
    if !t.running.is_empty() {
        let _ = writeln!(
            out,
            "{:<8} {:<10} {:<12} {:>7} {:<9} {:<13} {:>8}  COMMAND",
            "KIND", "AGENT", "PROJECT", "AGE", "CPU act/exp", "MEM act/exp GiB", "IO"
        );
    }
    for v in &t.running {
        let _ = writeln!(out, "{}", running_row(v));
    }
    let _ = writeln!(out, "\nQUEUED ({})", t.queued.len());
    if !t.queued.is_empty() {
        let _ = writeln!(
            out,
            "{:<8} {:<10} {:<12} {:>7} {:<14} {:>6}  COMMAND",
            "KIND", "AGENT", "PROJECT", "WAITED", "WAITING ON", "MEM"
        );
    }
    for v in &t.queued {
        let _ = writeln!(out, "{}", queued_row(v));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crew_proto::DiskView;

    #[test]
    fn render_names_what_each_queued_job_waits_on() {
        let t = TopReply {
            running: vec![JobView {
                kind: JobKind::Build as i32,
                agent: "a1".into(),
                project: "horizon".into(),
                command: "cargo check".into(),
                age_ms: 65_000,
                cpu_usec: 130_000_000,
                expected_cores_milli: 4000,
                ..Default::default()
            }],
            queued: vec![JobView {
                kind: JobKind::Game as i32,
                command: "horizon-client".into(),
                waiting_on: WaitingOn::GameCap as i32,
                expected_mem_bytes: 6 << 30,
                ..Default::default()
            }],
            machine: Some(MachineView {
                mem_total: 64 << 30,
                mem_available: 40 << 30,
                cpu_count: 24,
                disk: vec![DiskView {
                    path: "/projects".into(),
                    free_bytes: 100 << 30,
                    min_free_bytes: 50 << 30,
                }],
                owner_present: true,
                owner_source: OwnerSource::Manual as i32,
                ..Default::default()
            }),
        };
        let s = render(&t);
        assert!(s.contains("RUNNING (1)") && s.contains("QUEUED (1)"), "{s}");
        assert!(
            s.contains("game cap") && s.contains("horizon-client"),
            "{s}"
        );
        assert!(s.contains("2.0/4.0"), "{s}");
        assert!(
            s.contains("40.0 of 64.0 GiB")
                && s.contains("/projects 100.0 GiB free (min 50.0)")
                && s.contains("present (manual)"),
            "{s}"
        );
    }

    #[test]
    fn empty_reply_renders() {
        let s = render(&TopReply::default());
        assert!(s.contains("RUNNING (0)") && s.contains("QUEUED (0)"));
    }
}
