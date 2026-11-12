use crew_proto::{WaitCondition, WaitOutcome};

/// What the daemon knows about one job id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobState {
    Queued,
    Granted,
    Refused,
    Gone,
    Unknown,
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

/// `Err` when the request itself is malformed; `Ok(None)` while the condition does not hold yet.
pub fn evaluate(
    condition: WaitCondition,
    job_id: &str,
    facts: &Facts<'_>,
) -> Result<Option<(WaitOutcome, String)>, String> {
    let met = |detail: &str| Ok(Some((WaitOutcome::Satisfied, detail.to_string())));
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
        WaitCondition::JobGranted => {
            if job_id.is_empty() {
                return Err("job_granted needs a job id".into());
            }
            match (facts.job)(job_id) {
                JobState::Granted => met("job was granted"),
                JobState::Refused => {
                    Ok(Some((WaitOutcome::NeverGranted, "job was refused".into())))
                }
                JobState::Gone => Ok(Some((
                    WaitOutcome::NeverGranted,
                    "job's client left before it was granted".into(),
                ))),
                JobState::Queued | JobState::Unknown => Ok(None),
            }
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
        let lookup = move |_: &str| job;
        let facts = Facts {
            queue_empty,
            owner_present,
            job: &lookup,
        };
        evaluate(c, id, &facts).map(|o| o.map(|(o, _)| o))
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
