use std::fmt::Write;

use crew_proto::{
    AccountingSource, AgentKind, AgentView, IdleWatcher, JobKind, JobView, MachineView,
    OwnerSource, ProjectView, TopReply, WaitingOn,
};

const GIB: f64 = (1u64 << 30) as f64;

fn gib(bytes: u64) -> String {
    format!("{:.1}", bytes as f64 / GIB)
}

pub(crate) fn duration(ms: u64) -> String {
    let s = ms / 1000;
    if s >= 3600 {
        format!("{}h{:02}m", s / 3600, s % 3600 / 60)
    } else if s >= 60 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        format!("{s}s")
    }
}

/// When a queued job should start: `~12m`, `now`, or `-` when the usher cannot tell.
pub(crate) fn starts_in(ms: Option<u64>) -> String {
    match ms {
        None => "-".to_string(),
        Some(0) => "now".to_string(),
        Some(ms) => format!("~{}", duration(ms)),
    }
}

pub(crate) fn truncate(s: &str, max: usize) -> String {
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
    let watcher = match IdleWatcher::try_from(m.idle_watcher).unwrap_or(IdleWatcher::Unspecified) {
        IdleWatcher::Watching | IdleWatcher::Unspecified => "",
        IdleWatcher::NotConfigured => "; idle detection is off",
        IdleWatcher::Lost => "; idle watcher lost, retrying",
        IdleWatcher::Unavailable => "; idle watcher unavailable, retrying",
    };
    let _ = writeln!(out, "owner    {owner} ({source}{watcher})");
    if !m.draining_for.is_empty() {
        let _ = writeln!(
            out,
            "drain    draining for {}: no new job starts until it has run",
            m.draining_for
        );
    }
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
        "{:<8} {:<10} {:<12} {:>7} {:<14} {:>7} {:>6}  {}",
        kind_name(v),
        truncate(&v.agent, 10),
        truncate(&v.project, 12),
        duration(v.age_ms),
        waiting.name(),
        starts_in(v.starts_in_ms),
        gib(v.expected_mem_bytes),
        truncate(&v.command, 60),
    )
}

fn mib(bytes: u64) -> String {
    format!("{:.0}M", bytes as f64 / (1u64 << 20) as f64)
}

fn agent_row(a: &AgentView) -> String {
    let kind = match AgentKind::try_from(a.kind).unwrap_or(AgentKind::Unspecified) {
        AgentKind::Codex => "codex",
        AgentKind::Claude => "claude",
        AgentKind::Unspecified => "?",
    };
    let source = match AccountingSource::try_from(a.source).unwrap_or(AccountingSource::Unspecified)
    {
        AccountingSource::Cgroup => "cgroup",
        AccountingSource::Proc => "proc",
        AccountingSource::Unspecified => "?",
    };
    let note = if a.has_tty {
        "terminal"
    } else if !a.named {
        "unnamed"
    } else if a.actionable {
        ""
    } else {
        "protected"
    };
    format!(
        "{:<14} {:<6} {:<12} {:>5} {:>5.1} {:>8} {:>7} {:>7} {:>7} {:<6} {}",
        truncate(&a.name, 14),
        kind,
        truncate(&a.project, 12),
        a.processes,
        f64::from(a.cpu_milli) / 1000.0,
        duration(a.cpu_usec / 1000),
        gib(a.mem_bytes),
        mib(a.io_read_bytes),
        mib(a.io_write_bytes),
        source,
        note,
    )
}

fn project_row(p: &ProjectView) -> String {
    format!(
        "{:<14} {:>6} {:>5} {:>5.1} {:>8} {:>7} {:>7} {:>7} {:>4}/{:<4}",
        truncate(&p.name, 14),
        p.agents,
        p.processes,
        f64::from(p.cpu_milli) / 1000.0,
        duration(p.cpu_usec / 1000),
        gib(p.mem_bytes),
        mib(p.io_read_bytes),
        mib(p.io_write_bytes),
        p.running_jobs,
        p.queued_jobs,
    )
}

fn agent_sections(t: &TopReply, out: &mut String) {
    if !t.agents.is_empty() {
        let _ = writeln!(out, "\nAGENTS ({})", t.agents.len());
        let _ = writeln!(
            out,
            "{:<14} {:<6} {:<12} {:>5} {:>5} {:>8} {:>7} {:>7} {:>7} {:<6} NOTE",
            "AGENT",
            "KIND",
            "PROJECT",
            "PROCS",
            "CPU",
            "CPU TIME",
            "MEM GiB",
            "READ",
            "WRITTEN",
            "SOURCE"
        );
        for a in &t.agents {
            let _ = writeln!(out, "{}", agent_row(a));
        }
    }
    if !t.projects.is_empty() {
        let _ = writeln!(out, "\nPROJECTS ({})", t.projects.len());
        let _ = writeln!(
            out,
            "{:<14} {:>6} {:>5} {:>5} {:>8} {:>7} {:>7} {:>7} JOBS run/queued",
            "PROJECT", "AGENTS", "PROCS", "CPU", "CPU TIME", "MEM GiB", "READ", "WRITTEN"
        );
        for p in &t.projects {
            let _ = writeln!(out, "{}", project_row(p));
        }
    }
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
            "{:<8} {:<10} {:<12} {:>7} {:<14} {:>7} {:>6}  COMMAND",
            "KIND", "AGENT", "PROJECT", "WAITED", "WAITING ON", "STARTS", "MEM"
        );
    }
    for v in &t.queued {
        let _ = writeln!(out, "{}", queued_row(v));
    }
    agent_sections(t, &mut out);
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
                project: "myrepo".into(),
                command: "cargo check".into(),
                age_ms: 65_000,
                cpu_usec: 130_000_000,
                expected_cores_milli: 4000,
                ..Default::default()
            }],
            queued: vec![
                JobView {
                    kind: JobKind::Game as i32,
                    command: "my-game".into(),
                    waiting_on: WaitingOn::GameCap as i32,
                    expected_mem_bytes: 6 << 30,
                    ..Default::default()
                },
                JobView {
                    kind: JobKind::Test as i32,
                    command: "cargo test".into(),
                    waiting_on: WaitingOn::Drain as i32,
                    starts_in_ms: Some(12 * 60_000),
                    ..Default::default()
                },
            ],
            machine: Some(MachineView {
                mem_total: 64 << 30,
                mem_available: 40 << 30,
                cpu_count: 24,
                disk: vec![DiskView {
                    path: "/data".into(),
                    free_bytes: 100 << 30,
                    min_free_bytes: 50 << 30,
                }],
                owner_present: true,
                owner_source: OwnerSource::Manual as i32,
                draining_for: "bench 1a2b3c4d (myrepo: ./time.sh)".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let s = render(&t);
        assert!(s.contains("RUNNING (1)") && s.contains("QUEUED (2)"), "{s}");
        assert!(s.contains("game cap") && s.contains("my-game"), "{s}");
        let game = s.lines().find(|l| l.contains("my-game")).unwrap();
        assert!(
            game.contains(" - "),
            "no estimate without durations: {game}"
        );
        let test = s.lines().find(|l| l.contains("cargo test")).unwrap();
        assert!(test.contains("drain") && test.contains("~12m00s"), "{test}");
        assert!(s.contains("draining for bench 1a2b3c4d"), "{s}");
        assert!(s.contains("2.0/4.0"), "{s}");
        assert!(
            s.contains("40.0 of 64.0 GiB")
                && s.contains("/data 100.0 GiB free (min 50.0)")
                && s.contains("present (manual)"),
            "{s}"
        );
    }

    #[test]
    fn a_lost_idle_watcher_shows_next_to_the_owner() {
        let machine = |w: IdleWatcher| MachineView {
            owner_present: true,
            owner_source: OwnerSource::Default as i32,
            idle_watcher: w as i32,
            ..Default::default()
        };
        let render_with = |w| {
            render(&TopReply {
                machine: Some(machine(w)),
                ..Default::default()
            })
        };
        assert!(render_with(IdleWatcher::Lost).contains("idle watcher lost, retrying"));
        assert!(render_with(IdleWatcher::Unavailable).contains("idle watcher unavailable"));
        assert!(!render_with(IdleWatcher::Watching).contains("watcher"));
    }

    #[test]
    fn agents_and_projects_render_with_their_accounting_and_protection() {
        let agent = |name: &str, tty: bool, actionable: bool, source: AccountingSource| AgentView {
            name: name.into(),
            kind: AgentKind::Claude as i32,
            project: "myrepo".into(),
            processes: 7,
            cpu_milli: 2500,
            cpu_usec: 90_000_000,
            mem_bytes: 3 << 30,
            io_read_bytes: 5 << 20,
            io_write_bytes: 7 << 20,
            source: source as i32,
            has_tty: tty,
            named: name != "claude-9",
            actionable,
            ..Default::default()
        };
        let t = TopReply {
            agents: vec![
                agent("t1", false, true, AccountingSource::Cgroup),
                agent("claude-9", true, false, AccountingSource::Proc),
                agent("lead", false, false, AccountingSource::Proc),
            ],
            projects: vec![ProjectView {
                name: "myrepo".into(),
                agents: 3,
                processes: 21,
                running_jobs: 2,
                queued_jobs: 1,
                ..Default::default()
            }],
            ..Default::default()
        };
        let s = render(&t);
        let line = |name: &str| {
            s.lines()
                .find(|l| l.starts_with(name))
                .unwrap_or_default()
                .to_string()
        };
        assert!(
            s.contains("AGENTS (3)") && s.contains("PROJECTS (1)"),
            "{s}"
        );
        let t1 = line("t1");
        assert!(
            t1.contains("cgroup") && t1.contains("2.5") && t1.contains("3.0") && t1.contains("5M"),
            "{t1}"
        );
        assert!(line("claude-9").ends_with("terminal"), "{s}");
        assert!(line("lead").ends_with("protected"), "{s}");
        assert!(line("myrepo").contains("2/1"), "{s}");
    }

    #[test]
    fn empty_reply_renders() {
        let s = render(&TopReply::default());
        assert!(s.contains("RUNNING (0)") && s.contains("QUEUED (0)"));
    }
}
