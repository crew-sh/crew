use crew_proto::{WaitCondition, WaitOutcome};

/// What the daemon knows about one job id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JobState {
    Queued,
    Granted,
    Refused,
    Gone,
    Unknown,
    /// A detached job waiting for admission.
    DetachedQueued,
    DetachedRunning,
    DetachedExited(i32),
    /// A detached job that ended without an exit status; `ran` says whether it ever started.
    DetachedEnded {
        ran: bool,
        reason: String,
    },
}

/// Why a job id left the queue; kept for a while so late waiters still get an answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fate {
    Granted,
    Refused,
    Gone,
}

pub struct Facts<'a> {
    pub queue_empty: bool,
    pub owner_present: bool,
    pub job: &'a dyn Fn(&str) -> JobState,
}

/// A wait that holds now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verdict {
    pub outcome: WaitOutcome,
    pub detail: String,
    pub exit_code: Option<i32>,
}

impl Verdict {
    fn new(outcome: WaitOutcome, detail: impl Into<String>) -> Self {
        Self {
            outcome,
            detail: detail.into(),
            exit_code: None,
        }
    }
}

fn granted(job: JobState) -> Option<Verdict> {
    match job {
        JobState::Granted | JobState::DetachedRunning | JobState::DetachedExited(_) => {
            Some(Verdict::new(WaitOutcome::Satisfied, "job was granted"))
        }
        JobState::DetachedEnded { ran: true, .. } => {
            Some(Verdict::new(WaitOutcome::Satisfied, "job was granted"))
        }
        JobState::Refused => Some(Verdict::new(WaitOutcome::NeverGranted, "job was refused")),
        JobState::Gone => Some(Verdict::new(
            WaitOutcome::NeverGranted,
            "job's client left before it was granted",
        )),
        JobState::DetachedEnded { ran: false, reason } => {
            Some(Verdict::new(WaitOutcome::NeverGranted, reason))
        }
        JobState::Queued | JobState::DetachedQueued | JobState::Unknown => None,
    }
}

/// A detached job's end; any other job counts once it was granted, since its client owns the exit code.
fn exited(job: JobState) -> Option<Verdict> {
    match job {
        JobState::DetachedExited(code) => Some(Verdict {
            exit_code: Some(code),
            ..Verdict::new(
                WaitOutcome::Satisfied,
                format!("job exited with status {code}"),
            )
        }),
        JobState::DetachedEnded { ran: true, reason } => {
            Some(Verdict::new(WaitOutcome::JobLost, reason))
        }
        JobState::DetachedRunning => None,
        other => granted(other),
    }
}

/// `Err` when the request itself is malformed; `Ok(None)` while the condition does not hold yet.
pub fn evaluate(
    condition: WaitCondition,
    job_id: &str,
    facts: &Facts<'_>,
) -> Result<Option<Verdict>, String> {
    let met = |detail: &str| Ok(Some(Verdict::new(WaitOutcome::Satisfied, detail)));
    match condition {
        WaitCondition::Unspecified => Err("no wait condition given".into()),
        WaitCondition::QueueEmpty => {
            if facts.queue_empty {
                met("queue is empty")
            } else {
                Ok(None)
            }
        }
        WaitCondition::OwnerAway => {
            if facts.owner_present {
                Ok(None)
            } else {
                met("owner is away")
            }
        }
        WaitCondition::JobGranted | WaitCondition::JobExit => {
            if job_id.is_empty() {
                return Err("a job wait needs a job id".into());
            }
            let state = (facts.job)(job_id);
            Ok(if condition == WaitCondition::JobExit {
                exited(state)
            } else {
                granted(state)
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(
        c: WaitCondition,
        id: &str,
        queue_empty: bool,
        owner_present: bool,
        job: JobState,
    ) -> Result<Option<WaitOutcome>, String> {
        let lookup = move |_: &str| job.clone();
        let facts = Facts {
            queue_empty,
            owner_present,
            job: &lookup,
        };
        evaluate(c, id, &facts).map(|o| o.map(|v| v.outcome))
    }

    #[test]
    fn each_condition_holds_only_when_it_should() {
        use WaitCondition::*;
        let sat = Ok(Some(WaitOutcome::Satisfied));
        assert_eq!(run(QueueEmpty, "", true, true, JobState::Unknown), sat);
        assert_eq!(
            run(QueueEmpty, "", false, true, JobState::Unknown),
            Ok(None)
        );
        assert_eq!(run(OwnerAway, "", true, false, JobState::Unknown), sat);
        assert_eq!(run(OwnerAway, "", true, true, JobState::Unknown), Ok(None));
        assert_eq!(run(JobGranted, "j", false, true, JobState::Granted), sat);
        assert_eq!(
            run(JobGranted, "j", false, true, JobState::Queued),
            Ok(None)
        );
        assert_eq!(
            run(JobGranted, "j", false, true, JobState::Unknown),
            Ok(None)
        );
        let never = Ok(Some(WaitOutcome::NeverGranted));
        assert_eq!(run(JobGranted, "j", false, true, JobState::Refused), never);
        assert_eq!(run(JobGranted, "j", false, true, JobState::Gone), never);
    }

    #[test]
    fn a_detached_job_is_waited_for_until_it_exits() {
        use WaitCondition::*;
        let exit = |state: JobState| {
            let lookup = move |_: &str| -> JobState { state.clone() };
            let facts = Facts {
                queue_empty: true,
                owner_present: true,
                job: &lookup,
            };
            evaluate(JobExit, "j", &facts).unwrap()
        };
        assert_eq!(exit(JobState::DetachedQueued), None);
        assert_eq!(exit(JobState::DetachedRunning), None);
        let done = exit(JobState::DetachedExited(3)).unwrap();
        assert_eq!(
            (done.outcome, done.exit_code),
            (WaitOutcome::Satisfied, Some(3))
        );
        let lost = exit(JobState::DetachedEnded {
            ran: true,
            reason: "scope gone".into(),
        })
        .unwrap();
        assert_eq!((lost.outcome, lost.exit_code), (WaitOutcome::JobLost, None));
        let never = exit(JobState::DetachedEnded {
            ran: false,
            reason: "cancelled".into(),
        })
        .unwrap();
        assert_eq!(never.outcome, WaitOutcome::NeverGranted);
        // The older condition still means "started": a running detached job satisfies it.
        assert_eq!(
            run(JobGranted, "j", true, true, JobState::DetachedRunning),
            Ok(Some(WaitOutcome::Satisfied))
        );
        assert_eq!(exit(JobState::Granted).unwrap().exit_code, None);
    }

    #[test]
    fn malformed_requests_are_rejected() {
        assert!(
            run(
                WaitCondition::Unspecified,
                "",
                true,
                true,
                JobState::Unknown
            )
            .is_err()
        );
        assert!(run(WaitCondition::JobGranted, "", true, true, JobState::Granted).is_err());
    }
}
