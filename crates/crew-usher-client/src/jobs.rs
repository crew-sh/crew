//! `crew jobs` text: queued, running and recently finished jobs, with exit codes and log paths.
use std::fmt::Write;

use crew_proto::{JobKind, JobPhase, JobSummary, JobsReply, WaitingOn};

use crate::top::{duration, starts_in, truncate};

fn phase(j: &JobSummary) -> JobPhase {
    JobPhase::try_from(j.phase).unwrap_or(JobPhase::Unspecified)
}

fn kind(j: &JobSummary) -> &'static str {
    JobKind::try_from(j.kind)
        .unwrap_or(JobKind::Unspecified)
        .name()
}

fn owner(j: &JobSummary) -> &'static str {
    if j.detached { "usher" } else { "client" }
}

fn ago(now_ms: u64, at_ms: u64) -> String {
    duration(now_ms.saturating_sub(at_ms))
}

fn result(j: &JobSummary) -> String {
    match (phase(j), j.exit_code) {
        (JobPhase::Exited, Some(c)) => format!("exit {c}"),
        (JobPhase::Cancelled, _) => "cancelled".to_string(),
        (JobPhase::Failed, _) => "failed".to_string(),
        (_, Some(c)) => format!("exit {c}"),
        _ => "-".to_string(),
    }
}

fn section(
    out: &mut String,
    title: &str,
    jobs: &[&JobSummary],
    row: impl Fn(&JobSummary) -> String,
) {
    if jobs.is_empty() {
        return;
    }
    let _ = writeln!(out, "{title} ({})", jobs.len());
    for j in jobs {
        let _ = writeln!(out, "{}", row(j));
        if !j.log_path.is_empty() {
            let _ = writeln!(out, "    log: {}", j.log_path);
        }
        if !j.reason.is_empty() {
            let _ = writeln!(out, "    {}", j.reason);
        }
    }
    let _ = writeln!(out);
}

/// `now_ms` is the current unix time in milliseconds, for the ages.
pub fn render(r: &JobsReply, now_ms: u64) -> String {
    let by =
        |p: JobPhase| -> Vec<&JobSummary> { r.jobs.iter().filter(|j| phase(j) == p).collect() };
    let mut out = String::new();
    section(&mut out, "QUEUED", &by(JobPhase::Queued), |j| {
        let waiting = WaitingOn::try_from(j.waiting_on).unwrap_or(WaitingOn::Unspecified);
        format!(
            "{}  {:<8} {:<6} {:<10} {:<12} waited {:>7}  on {:<13} starts {:>7}  {}",
            j.id,
            kind(j),
            owner(j),
            truncate(&j.agent, 10),
            truncate(&j.project, 12),
            ago(now_ms, j.queued_at_unix_ms),
            waiting.name(),
            starts_in(j.starts_in_ms),
            truncate(&j.command, 50),
        )
    });
    section(&mut out, "RUNNING", &by(JobPhase::Running), |j| {
        format!(
            "{}  {:<8} {:<6} {:<10} {:<12} ran {:>7}  {}",
            j.id,
            kind(j),
            owner(j),
            truncate(&j.agent, 10),
            truncate(&j.project, 12),
            ago(now_ms, j.started_at_unix_ms),
            truncate(&j.command, 50),
        )
    });
    let finished: Vec<&JobSummary> = r
        .jobs
        .iter()
        .filter(|j| {
            matches!(
                phase(j),
                JobPhase::Exited | JobPhase::Failed | JobPhase::Cancelled
            )
        })
        .collect();
    section(&mut out, "FINISHED", &finished, |j| {
        format!(
            "{}  {:<8} {:<10} {:<12} {:<9} {:>7} ago  {}",
            j.id,
            kind(j),
            truncate(&j.agent, 10),
            truncate(&j.project, 12),
            result(j),
            ago(now_ms, j.finished_at_unix_ms),
            truncate(&j.command, 50),
        )
    });
    if r.jobs.is_empty() {
        out.push_str("no jobs\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(id: &str, phase: JobPhase) -> JobSummary {
        JobSummary {
            id: id.into(),
            kind: JobKind::Bench as i32,
            phase: phase as i32,
            command: "./m.sh".into(),
            detached: true,
            ..JobSummary::default()
        }
    }

    #[test]
    fn each_phase_gets_a_section_with_its_exit_code_and_log() {
        let r = JobsReply {
            jobs: vec![
                JobSummary {
                    reason: "memory rule".into(),
                    waiting_on: WaitingOn::Memory as i32,
                    starts_in_ms: Some(90_000),
                    ..job("q1", JobPhase::Queued)
                },
                job("r1", JobPhase::Running),
                JobSummary {
                    exit_code: Some(3),
                    log_path: "/state/jobs/e1.log".into(),
                    ..job("e1", JobPhase::Exited)
                },
                job("c1", JobPhase::Cancelled),
            ],
        };
        let text = render(&r, 0);
        for part in [
            "QUEUED (1)",
            "RUNNING (1)",
            "FINISHED (2)",
            "exit 3",
            "log: /state/jobs/e1.log",
            "cancelled",
            "memory rule",
            "on memory",
            "starts  ~1m30s",
        ] {
            assert!(text.contains(part), "{part} missing from\n{text}");
        }
        assert_eq!(render(&JobsReply::default(), 0), "no jobs\n");
    }
}

/// What `crew` exits with when a job reports an exit code outside 0..=255: a crew error, never a masked (and possibly "success") value.
pub const EXIT_ERROR: u8 = 125;

/// A job's exit code as a process exit status.
pub fn exit_byte(code: i32) -> u8 {
    u8::try_from(code).unwrap_or(EXIT_ERROR)
}

#[cfg(test)]
mod exit_tests {
    use super::*;

    #[test]
    fn codes_in_range_pass_through_and_others_are_errors_not_masked() {
        assert_eq!([0, 1, 2, 137, 255].map(exit_byte), [0, 1, 2, 137, 255]);
        for out_of_range in [256, 512, 0x1_0000, -1, i32::MIN, i32::MAX] {
            assert_eq!(exit_byte(out_of_range), EXIT_ERROR, "{out_of_range}");
        }
    }
}
