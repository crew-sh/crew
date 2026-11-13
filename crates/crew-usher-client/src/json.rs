//! Typed JSON for every client-visible record: mirrors of the socket messages, enums as snake_case names.
use crew_proto::{
    AccountingSource, AgentKind, AgentView, DiskView, IdleWatcher, JobKind, JobPhase, JobSummary,
    JobView, JobsReply, MachineView, MemoryRetry, OwnerSource, ProjectView, RefusalReason,
    TopReply, WaitCondition, WaitOutcome, WaitReply, WaitingOn,
};
use serde::Serialize;

macro_rules! mirror {
    ($name:ident <= $proto:ident { $($v:ident),+ $(,)? }) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name { $($v),+ }

        impl From<i32> for $name {
            fn from(raw: i32) -> Self {
                match $proto::try_from(raw).unwrap_or($proto::Unspecified) {
                    $($proto::$v => Self::$v),+
                }
            }
        }
    };
}

mirror!(
    Kind <= JobKind {
        Unspecified,
        Build,
        Test,
        Harness,
        Game,
        Bench,
        Lsp,
        Other
    }
);
mirror!(
    Waiting
        <= WaitingOn {
            Unspecified,
            Memory,
            Cpu,
            GameCap,
            OwnerPresent,
            Disk,
            Exclusive,
            Drain
        }
);
mirror!(
    Owner
        <= OwnerSource {
            Unspecified,
            Manual,
            WaylandIdle,
            Auto,
            Default
        }
);
mirror!(
    Watcher
        <= IdleWatcher {
            Unspecified,
            NotConfigured,
            Watching,
            Lost,
            Unavailable
        }
);
mirror!(
    Agent
        <= AgentKind {
            Unspecified,
            Claude,
            Codex
        }
);
mirror!(
    Accounting
        <= AccountingSource {
            Unspecified,
            Proc,
            Cgroup
        }
);
mirror!(
    Refusal
        <= RefusalReason {
            Unspecified,
            DiskLow,
            OverGameCap,
            DisplayForbidden,
            UnknownKind,
            Cancelled,
            TooLarge
        }
);
mirror!(
    Condition
        <= WaitCondition {
            Unspecified,
            QueueEmpty,
            JobGranted,
            OwnerAway,
            JobExit
        }
);
mirror!(
    Outcome
        <= WaitOutcome {
            Unspecified,
            Satisfied,
            TimedOut,
            Rejected,
            NeverGranted,
            JobLost
        }
);
mirror!(
    Phase
        <= JobPhase {
            Unspecified,
            Queued,
            Running,
            Exited,
            Failed,
            Cancelled
        }
);

/// The usher killed an earlier run of the job for memory and queued it again.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Retry {
    pub kills: u32,
    pub expected_mem_bytes: u64,
    pub peak_mem_bytes: u64,
}

impl From<&MemoryRetry> for Retry {
    fn from(r: &MemoryRetry) -> Self {
        Self {
            kills: r.kills,
            expected_mem_bytes: r.expected_mem_bytes,
            peak_mem_bytes: r.peak_mem_bytes,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Job {
    pub id: String,
    pub kind: Kind,
    pub agent: String,
    pub project: String,
    pub command: String,
    pub expected_mem_bytes: u64,
    pub expected_cores_milli: u32,
    pub age_ms: u64,
    pub cpu_usec: u64,
    pub mem_bytes: u64,
    pub io_bytes: u64,
    pub waiting_on: Waiting,
    /// Queued jobs: when the job should start, from learned durations.
    pub starts_in_ms: Option<u64>,
    /// Queued jobs: what the wait is, with the numbers.
    pub detail: String,
    pub retry: Option<Retry>,
}

impl From<&JobView> for Job {
    fn from(v: &JobView) -> Self {
        Self {
            id: v.id.clone(),
            kind: v.kind.into(),
            agent: v.agent.clone(),
            project: v.project.clone(),
            command: v.command.clone(),
            expected_mem_bytes: v.expected_mem_bytes,
            expected_cores_milli: v.expected_cores_milli,
            age_ms: v.age_ms,
            cpu_usec: v.cpu_usec,
            mem_bytes: v.mem_bytes,
            io_bytes: v.io_bytes,
            waiting_on: v.waiting_on.into(),
            starts_in_ms: v.starts_in_ms,
            detail: v.detail.clone(),
            retry: v.retry.as_ref().map(Retry::from),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Disk {
    pub path: String,
    pub free_bytes: u64,
    pub min_free_bytes: u64,
}

impl From<&DiskView> for Disk {
    fn from(d: &DiskView) -> Self {
        Self {
            path: d.path.clone(),
            free_bytes: d.free_bytes,
            min_free_bytes: d.min_free_bytes,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Machine {
    pub mem_total: u64,
    pub mem_available: u64,
    pub cpu_count: u32,
    pub load1: f64,
    pub psi_cpu_some_avg10: f64,
    pub psi_mem_some_avg10: f64,
    pub psi_io_some_avg10: f64,
    pub disk: Vec<Disk>,
    pub owner_present: bool,
    pub owner_source: Owner,
    pub idle_watcher: Watcher,
    pub unmanaged_cpu_pct: f64,
    /// The exclusive job the usher drains the machine for; empty when none.
    pub draining_for: String,
}

impl From<&MachineView> for Machine {
    fn from(m: &MachineView) -> Self {
        Self {
            mem_total: m.mem_total,
            mem_available: m.mem_available,
            cpu_count: m.cpu_count,
            load1: m.load1,
            psi_cpu_some_avg10: m.psi_cpu_some_avg10,
            psi_mem_some_avg10: m.psi_mem_some_avg10,
            psi_io_some_avg10: m.psi_io_some_avg10,
            disk: m.disk.iter().map(Disk::from).collect(),
            owner_present: m.owner_present,
            owner_source: m.owner_source.into(),
            idle_watcher: m.idle_watcher.into(),
            unmanaged_cpu_pct: m.unmanaged_cpu_pct,
            draining_for: m.draining_for.clone(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct AgentSession {
    pub name: String,
    pub kind: Agent,
    pub pid: u32,
    pub project: String,
    pub cwd: String,
    pub processes: u32,
    pub cpu_usec: u64,
    pub cpu_milli: u32,
    pub mem_bytes: u64,
    pub io_read_bytes: u64,
    pub io_write_bytes: u64,
    pub cgroup: String,
    pub source: Accounting,
    pub age_ms: u64,
    pub named: bool,
    pub has_tty: bool,
    pub actionable: bool,
}

impl From<&AgentView> for AgentSession {
    fn from(a: &AgentView) -> Self {
        Self {
            name: a.name.clone(),
            kind: a.kind.into(),
            pid: a.pid,
            project: a.project.clone(),
            cwd: a.cwd.clone(),
            processes: a.processes,
            cpu_usec: a.cpu_usec,
            cpu_milli: a.cpu_milli,
            mem_bytes: a.mem_bytes,
            io_read_bytes: a.io_read_bytes,
            io_write_bytes: a.io_write_bytes,
            cgroup: a.cgroup.clone(),
            source: a.source.into(),
            age_ms: a.age_ms,
            named: a.named,
            has_tty: a.has_tty,
            actionable: a.actionable,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Project {
    pub name: String,
    pub toplevel: String,
    pub agents: u32,
    pub processes: u32,
    pub cpu_usec: u64,
    pub cpu_milli: u32,
    pub mem_bytes: u64,
    pub io_read_bytes: u64,
    pub io_write_bytes: u64,
    pub running_jobs: u32,
    pub queued_jobs: u32,
}

impl From<&ProjectView> for Project {
    fn from(p: &ProjectView) -> Self {
        Self {
            name: p.name.clone(),
            toplevel: p.toplevel.clone(),
            agents: p.agents,
            processes: p.processes,
            cpu_usec: p.cpu_usec,
            cpu_milli: p.cpu_milli,
            mem_bytes: p.mem_bytes,
            io_read_bytes: p.io_read_bytes,
            io_write_bytes: p.io_write_bytes,
            running_jobs: p.running_jobs,
            queued_jobs: p.queued_jobs,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Top {
    pub machine: Option<Machine>,
    pub running: Vec<Job>,
    pub queued: Vec<Job>,
    pub agents: Vec<AgentSession>,
    pub projects: Vec<Project>,
}

impl From<&TopReply> for Top {
    fn from(t: &TopReply) -> Self {
        Self {
            machine: t.machine.as_ref().map(Machine::from),
            running: t.running.iter().map(Job::from).collect(),
            queued: t.queued.iter().map(Job::from).collect(),
            agents: t.agents.iter().map(AgentSession::from).collect(),
            projects: t.projects.iter().map(Project::from).collect(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Wait {
    pub outcome: Outcome,
    pub condition: Condition,
    pub detail: String,
    pub waited_ms: u64,
    /// The finished detached job's exit status.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
}

impl From<&WaitReply> for Wait {
    fn from(w: &WaitReply) -> Self {
        Self {
            outcome: w.outcome.into(),
            condition: w.condition.into(),
            detail: w.detail.clone(),
            waited_ms: w.waited_ms,
            exit_code: w.exit_code,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Jobs {
    pub jobs: Vec<JobRecord>,
}

/// One job of `crew jobs`: queued, running or recently finished.
#[derive(Debug, Serialize)]
pub struct JobRecord {
    pub id: String,
    pub kind: Kind,
    pub agent: String,
    pub project: String,
    pub command: String,
    pub phase: Phase,
    pub detached: bool,
    pub exit_code: Option<i32>,
    pub log_path: String,
    pub reason: String,
    pub waiting_on: Waiting,
    pub position: u32,
    pub queued_at_unix_ms: u64,
    pub started_at_unix_ms: u64,
    pub finished_at_unix_ms: u64,
    /// Queued jobs: when the job should start, from learned durations.
    pub starts_in_ms: Option<u64>,
    pub retry: Option<Retry>,
}

impl From<&JobSummary> for JobRecord {
    fn from(j: &JobSummary) -> Self {
        Self {
            id: j.id.clone(),
            kind: j.kind.into(),
            agent: j.agent.clone(),
            project: j.project.clone(),
            command: j.command.clone(),
            phase: j.phase.into(),
            detached: j.detached,
            exit_code: j.exit_code,
            log_path: j.log_path.clone(),
            reason: j.reason.clone(),
            waiting_on: j.waiting_on.into(),
            position: j.position,
            queued_at_unix_ms: j.queued_at_unix_ms,
            started_at_unix_ms: j.started_at_unix_ms,
            finished_at_unix_ms: j.finished_at_unix_ms,
            starts_in_ms: j.starts_in_ms,
            retry: j.retry.as_ref().map(Retry::from),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Owned {
    pub ok: bool,
    pub message: String,
}

/// One line of `crew run --json` on stderr; the command's own output stays untouched on stdout.
#[derive(Debug, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum RunEvent {
    Queued {
        position: u32,
        queued_ms: u64,
        waiting_on: Waiting,
        detail: String,
    },
    Granted {
        job_id: String,
        scope: String,
    },
    Done {
        job_id: String,
        exit_code: i32,
        wall_ms: u64,
        cpu_usec: u64,
        peak_mem_bytes: u64,
        io_read_bytes: u64,
        io_write_bytes: u64,
    },
    /// The command ran without admission: the usher is down, or it was already inside a granted job.
    Unmanaged {
        reason: UnmanagedReason,
    },
    /// `--detach`: the usher owns the job now, and the client is done.
    Detached {
        job_id: String,
        log_path: String,
    },
    /// The usher went away while the job was queued; the client is waiting for it to return.
    UsherLost {
        job_id: String,
    },
    /// The usher is back and the job is queued again under its old id and time.
    Requeued {
        job_id: String,
    },
    /// The usher killed the run because memory ran low; the job is queued again and the command reruns once granted.
    MemoryKilled {
        job_id: String,
        kills: u32,
        peak_mem_bytes: u64,
        expected_mem_bytes: u64,
    },
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UnmanagedReason {
    UsherDown,
    InsideGrantedJob,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    UsherDown,
    Refused,
    Closed,
    Protocol,
    Spawn,
    Usage,
    Io,
    ProjectConfig,
    Rejected,
}

#[derive(Debug, Serialize)]
pub struct Failure {
    pub event: &'static str,
    pub kind: ErrorKind,
    pub refusal: Option<Refusal>,
    pub message: String,
}

fn line<T: Serialize>(v: &T) -> String {
    serde_json::to_string(v).expect("plain records serialize")
}

pub fn top(t: &TopReply) -> String {
    line(&Top::from(t))
}

pub fn wait(w: &WaitReply) -> String {
    line(&Wait::from(w))
}

pub fn jobs(r: &JobsReply) -> String {
    line(&Jobs {
        jobs: r.jobs.iter().map(JobRecord::from).collect(),
    })
}

pub fn ack(a: &crew_proto::Ack) -> String {
    line(&Owned {
        ok: a.ok,
        message: a.message.clone(),
    })
}

pub fn owner(message: &str) -> String {
    line(&Owned {
        ok: true,
        message: message.to_string(),
    })
}

pub fn event(e: &RunEvent) -> String {
    line(e)
}

pub fn failure(e: &crate::ClientError) -> String {
    use crate::ClientError as E;
    let (kind, refusal) = match e {
        E::DaemonDown { .. } | E::UsherLost { .. } => (ErrorKind::UsherDown, None),
        E::Rejected(_) => (ErrorKind::Rejected, None),
        E::Refused { reason, .. } => (ErrorKind::Refused, Some(Refusal::from(*reason as i32))),
        E::Closed => (ErrorKind::Closed, None),
        E::Protocol | E::Frame(_) => (ErrorKind::Protocol, None),
        E::Spawn { .. } => (ErrorKind::Spawn, None),
        E::EmptyCommand => (ErrorKind::Usage, None),
        E::Io(_) => (ErrorKind::Io, None),
        E::ProjectRules(_) => (ErrorKind::ProjectConfig, None),
    };
    line(&Failure {
        event: "error",
        kind,
        refusal,
        message: e.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct TopBack {
        machine: MachineBack,
        running: Vec<JobBack>,
        queued: Vec<JobBack>,
        agents: Vec<AgentBack>,
        projects: Vec<ProjectBack>,
    }

    #[derive(Deserialize)]
    struct MachineBack {
        owner_source: String,
        cpu_count: u32,
        disk: Vec<DiskBack>,
    }

    #[derive(Deserialize)]
    struct DiskBack {
        path: String,
        free_bytes: u64,
    }

    #[derive(Deserialize)]
    struct JobBack {
        kind: String,
        waiting_on: String,
        expected_mem_bytes: u64,
    }

    #[derive(Deserialize)]
    struct AgentBack {
        name: String,
        kind: String,
        source: String,
        actionable: bool,
        cpu_milli: u32,
    }

    #[derive(Deserialize)]
    struct ProjectBack {
        name: String,
        running_jobs: u32,
    }

    #[test]
    fn top_serializes_enums_by_name_and_keeps_every_record() {
        let t = TopReply {
            running: vec![JobView {
                kind: JobKind::Build as i32,
                ..Default::default()
            }],
            queued: vec![JobView {
                kind: JobKind::Game as i32,
                waiting_on: WaitingOn::GameCap as i32,
                expected_mem_bytes: 6 << 30,
                ..Default::default()
            }],
            machine: Some(MachineView {
                cpu_count: 24,
                owner_source: OwnerSource::WaylandIdle as i32,
                disk: vec![DiskView {
                    path: "/data".into(),
                    free_bytes: 5,
                    min_free_bytes: 1,
                }],
                ..Default::default()
            }),
            agents: vec![AgentView {
                name: "t1".into(),
                kind: AgentKind::Codex as i32,
                source: AccountingSource::Cgroup as i32,
                actionable: true,
                cpu_milli: 1500,
                ..Default::default()
            }],
            projects: vec![ProjectView {
                name: "crew".into(),
                running_jobs: 2,
                ..Default::default()
            }],
        };
        let back: TopBack = serde_json::from_str(&top(&t)).unwrap();
        assert_eq!(
            (back.running[0].kind.as_str(), back.queued[0].kind.as_str()),
            ("build", "game")
        );
        assert_eq!(
            (
                back.queued[0].waiting_on.as_str(),
                back.queued[0].expected_mem_bytes
            ),
            ("game_cap", 6 << 30)
        );
        assert_eq!(
            (back.machine.owner_source.as_str(), back.machine.cpu_count),
            ("wayland_idle", 24)
        );
        assert_eq!(
            (
                back.machine.disk[0].path.as_str(),
                back.machine.disk[0].free_bytes
            ),
            ("/data", 5)
        );
        let a = &back.agents[0];
        assert_eq!(
            (
                a.name.as_str(),
                a.kind.as_str(),
                a.source.as_str(),
                a.actionable,
                a.cpu_milli
            ),
            ("t1", "codex", "cgroup", true, 1500)
        );
        assert_eq!(
            (
                back.projects[0].name.as_str(),
                back.projects[0].running_jobs
            ),
            ("crew", 2)
        );
    }

    #[test]
    fn unknown_enum_values_read_as_unspecified() {
        assert_eq!(Kind::from(99), Kind::Unspecified);
        assert_eq!(
            serde_json::to_string(&Kind::from(99)).unwrap(),
            "\"unspecified\""
        );
    }

    #[test]
    fn events_and_failures_are_tagged_lines() {
        let e = event(&RunEvent::Queued {
            position: 2,
            queued_ms: 31_000,
            waiting_on: Waiting::Memory,
            detail: "d".into(),
        });
        assert_eq!(
            e,
            r#"{"event":"queued","position":2,"queued_ms":31000,"waiting_on":"memory","detail":"d"}"#
        );
        assert_eq!(
            event(&RunEvent::Unmanaged {
                reason: UnmanagedReason::UsherDown
            }),
            r#"{"event":"unmanaged","reason":"usher_down"}"#
        );
        let f = failure(&crate::ClientError::Refused {
            reason: RefusalReason::DiskLow,
            detail: "/data".into(),
        });
        assert!(
            f.contains(r#""event":"error","kind":"refused","refusal":"disk_low""#),
            "{f}"
        );
    }

    #[test]
    fn wait_and_owner_records() {
        let w = wait(&WaitReply {
            outcome: WaitOutcome::TimedOut as i32,
            condition: WaitCondition::QueueEmpty as i32,
            detail: "timed out".into(),
            waited_ms: 5,
            exit_code: None,
        });
        assert_eq!(
            w,
            r#"{"outcome":"timed_out","condition":"queue_empty","detail":"timed out","waited_ms":5}"#
        );
        let exited = wait(&WaitReply {
            outcome: WaitOutcome::Satisfied as i32,
            condition: WaitCondition::JobExit as i32,
            detail: "job exited with status 3".into(),
            waited_ms: 7,
            exit_code: Some(3),
        });
        assert!(
            exited.contains(r#""condition":"job_exit""#) && exited.ends_with(r#""exit_code":3}"#),
            "{exited}"
        );
        assert_eq!(
            owner("owner away (manual)"),
            r#"{"ok":true,"message":"owner away (manual)"}"#
        );
    }
}
