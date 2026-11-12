use std::collections::BTreeMap;

use crew_proto::{JobKind, RefusalReason, WaitingOn};

use crate::config::{Config, GIB};

#[derive(Clone, Debug)]
pub struct Facts {
    pub mem_available: u64,
    pub cpu_count: u32,
    pub unmanaged_cores: f64,
    pub free_disk: Vec<(String, u64)>,
    pub owner_present: bool,
    pub now_ms: u64,
}

#[derive(Clone, Debug)]
pub struct Running {
    pub kind: JobKind,
    pub expected_mem: u64,
    pub expected_cores: f64,
    pub observed_peak: u64,
}

#[derive(Clone, Debug)]
pub struct Waiting {
    pub id: String,
    pub kind: JobKind,
    pub expected_mem: u64,
    pub expected_cores: f64,
    pub needs_display: bool,
    pub queued_at_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Grant,
    Refuse(RefusalReason, String),
    Wait(WaitingOn, String),
}

fn disk_gated(kind: JobKind) -> bool {
    matches!(kind, JobKind::Build | JobKind::Test | JobKind::Harness)
}

fn gib(bytes: u64) -> u64 {
    bytes / GIB
}

fn disk_problem(cfg: &Config, facts: &Facts) -> Option<String> {
    facts.free_disk.iter().find_map(|(path, free)| {
        let min = cfg.min_free_disk(path)?;
        (*free < min).then(|| format!("{path} has {} GiB free, needs {}", gib(*free), gib(min)))
    })
}

fn refusal(cfg: &Config, facts: &Facts, w: &Waiting) -> Option<(RefusalReason, String)> {
    if w.kind == JobKind::Unspecified {
        return Some((
            RefusalReason::UnknownKind,
            "the request carries no job kind".into(),
        ));
    }
    if w.needs_display && facts.owner_present {
        return Some((
            RefusalReason::DisplayForbidden,
            "the owner is present; displays are never granted".into(),
        ));
    }
    if w.kind == JobKind::Game && cfg.max_games == 0 {
        return Some((RefusalReason::OverGameCap, "max_games is 0".into()));
    }
    if disk_gated(w.kind) {
        return disk_problem(cfg, facts).map(|d| (RefusalReason::DiskLow, d));
    }
    None
}

/// One verdict per queue entry, in queue order; grants consume the capacity later entries see.
pub fn schedule(
    cfg: &Config,
    facts: &Facts,
    running: &[Running],
    queue: &[Waiting],
) -> Vec<(String, Verdict)> {
    let mut reserved: u64 = running
        .iter()
        .map(|r| r.expected_mem.saturating_sub(r.observed_peak))
        .sum();
    let mut cores: f64 = running.iter().map(|r| r.expected_cores).sum();
    let mut games = running.iter().filter(|r| r.kind == JobKind::Game).count() as u32;
    let mut active = running.len();
    let mut exclusive_running = running.iter().any(|r| cfg.kind(r.kind).exclusive);
    let cap = (f64::from(facts.cpu_count) * cfg.cpu_oversub - facts.unmanaged_cores).max(0.0);
    let mut starved: Option<WaitingOn> = None;
    let mut kind_blocked: BTreeMap<i32, WaitingOn> = BTreeMap::new();
    let starve_ms = cfg.starve_after_secs * 1000;
    let mut out = Vec::with_capacity(queue.len());

    for w in queue {
        if let Some((reason, detail)) = refusal(cfg, facts, w) {
            out.push((w.id.clone(), Verdict::Refuse(reason, detail)));
            continue;
        }
        let spec = cfg.kind(w.kind);
        let wait = if let Some(reason) = starved {
            Some((reason, "behind a job that has waited too long".to_string()))
        } else if exclusive_running {
            Some((
                WaitingOn::Exclusive,
                "an exclusive job is running".to_string(),
            ))
        } else if let Some(reason) = kind_blocked.get(&(w.kind as i32)) {
            Some((
                *reason,
                "behind an earlier job of the same kind".to_string(),
            ))
        } else if spec.exclusive && facts.owner_present {
            Some((
                WaitingOn::OwnerPresent,
                "exclusive jobs run only while the owner is away".to_string(),
            ))
        } else if spec.exclusive && active > 0 {
            Some((
                WaitingOn::Exclusive,
                format!("{active} other job(s) still running"),
            ))
        } else if w.kind == JobKind::Game && games >= cfg.max_games {
            Some((
                WaitingOn::GameCap,
                format!("{games} of {} games running", cfg.max_games),
            ))
        } else if facts.mem_available.saturating_sub(reserved) < cfg.min_free_mem() + w.expected_mem
        {
            let free = facts.mem_available.saturating_sub(reserved);
            Some((
                WaitingOn::Memory,
                format!(
                    "{} GiB free after reservations, needs {} + {} kept free",
                    gib(free),
                    gib(w.expected_mem),
                    cfg.min_free_mem_gib
                ),
            ))
        } else if active > 0 && cores + w.expected_cores > cap {
            Some((
                WaitingOn::Cpu,
                format!("{cores:.1} of {cap:.1} cores spoken for"),
            ))
        } else {
            None
        };

        match wait {
            None => {
                reserved += w.expected_mem;
                cores += w.expected_cores;
                games += u32::from(w.kind == JobKind::Game);
                active += 1;
                exclusive_running |= spec.exclusive;
                out.push((w.id.clone(), Verdict::Grant));
            }
            Some((reason, detail)) => {
                // Owner presence is not something a queue can wait out, so it never blocks the line.
                if reason != WaitingOn::OwnerPresent
                    && facts.now_ms.saturating_sub(w.queued_at_ms) >= starve_ms
                {
                    starved.get_or_insert(reason);
                }
                if reason != WaitingOn::Memory {
                    kind_blocked.entry(w.kind as i32).or_insert(reason);
                }
                out.push((w.id.clone(), Verdict::Wait(reason, detail)));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> Facts {
        Facts {
            mem_available: 64 * GIB,
            cpu_count: 24,
            unmanaged_cores: 0.0,
            free_disk: vec![("/data".into(), 500 * GIB), ("/".into(), 100 * GIB)],
            owner_present: false,
            now_ms: 0,
        }
    }

    fn job(id: &str, kind: JobKind, mem_gib: u64) -> Waiting {
        Waiting {
            id: id.into(),
            kind,
            expected_mem: mem_gib * GIB,
            expected_cores: 1.0,
            needs_display: false,
            queued_at_ms: 0,
        }
    }

    fn run(kind: JobKind, mem_gib: u64) -> Running {
        Running {
            kind,
            expected_mem: mem_gib * GIB,
            expected_cores: 1.0,
            observed_peak: 0,
        }
    }

    fn verdicts(cfg: &Config, f: &Facts, running: &[Running], queue: &[Waiting]) -> Vec<Verdict> {
        schedule(cfg, f, running, queue)
            .into_iter()
            .map(|(_, v)| v)
            .collect()
    }

    fn wait_reason(v: &Verdict) -> Option<WaitingOn> {
        match v {
            Verdict::Wait(r, _) => Some(*r),
            _ => None,
        }
    }

    #[test]
    fn memory_gate_keeps_sixteen_gib_free() {
        let cfg = Config::default();
        let mut f = facts();
        f.mem_available = 30 * GIB;
        // 30 free, 16 kept: two 6 GiB jobs fit (12), the third (18) does not.
        let q = [
            job("a", JobKind::Test, 6),
            job("b", JobKind::Test, 6),
            job("c", JobKind::Test, 6),
        ];
        let v = verdicts(&cfg, &f, &[], &q);
        assert_eq!(v[..2], [Verdict::Grant, Verdict::Grant]);
        assert_eq!(wait_reason(&v[2]), Some(WaitingOn::Memory));
    }

    #[test]
    fn running_jobs_reserve_memory_they_have_not_used_yet() {
        let cfg = Config::default();
        let mut f = facts();
        f.mem_available = 30 * GIB;
        let fresh = run(JobKind::Build, 10);
        let v = verdicts(
            &cfg,
            &f,
            std::slice::from_ref(&fresh),
            &[job("a", JobKind::Test, 6)],
        );
        assert_eq!(wait_reason(&v[0]), Some(WaitingOn::Memory));
        // Once it has peaked, mem_available already reflects it and nothing more is reserved.
        let peaked = Running {
            observed_peak: 10 * GIB,
            ..fresh
        };
        assert_eq!(
            verdicts(&cfg, &f, &[peaked], &[job("a", JobKind::Test, 6)]),
            [Verdict::Grant]
        );
    }

    #[test]
    fn small_job_passes_a_big_one_waiting_on_memory() {
        let cfg = Config::default();
        let mut f = facts();
        f.mem_available = 30 * GIB;
        let v = verdicts(
            &cfg,
            &f,
            &[],
            &[
                job("big", JobKind::Test, 20),
                job("small", JobKind::Test, 2),
            ],
        );
        assert_eq!(wait_reason(&v[0]), Some(WaitingOn::Memory));
        assert_eq!(v[1], Verdict::Grant);
    }

    #[test]
    fn a_job_queued_too_long_blocks_everything_behind_it() {
        let cfg = Config::default();
        let mut f = facts();
        f.mem_available = 30 * GIB;
        f.now_ms = 11 * 60 * 1000;
        let v = verdicts(
            &cfg,
            &f,
            &[],
            &[
                job("big", JobKind::Test, 20),
                job("small", JobKind::Test, 2),
            ],
        );
        assert_eq!(wait_reason(&v[0]), Some(WaitingOn::Memory));
        assert_eq!(wait_reason(&v[1]), Some(WaitingOn::Memory));
        f.now_ms = 9 * 60 * 1000;
        assert_eq!(
            verdicts(
                &cfg,
                &f,
                &[],
                &[
                    job("big", JobKind::Test, 20),
                    job("small", JobKind::Test, 2)
                ]
            )[1],
            Verdict::Grant
        );
    }

    #[test]
    fn at_most_max_games_run_and_the_rest_queue() {
        let cfg = Config {
            max_games: 2,
            ..Config::default()
        };
        let running = [run(JobKind::Game, 1)];
        let v = verdicts(
            &cfg,
            &facts(),
            &running,
            &[
                job("g2", JobKind::Game, 1),
                job("g3", JobKind::Game, 1),
                job("t", JobKind::Test, 1),
            ],
        );
        assert_eq!(v[0], Verdict::Grant);
        assert_eq!(wait_reason(&v[1]), Some(WaitingOn::GameCap));
        assert_eq!(v[2], Verdict::Grant);
    }

    #[test]
    fn zero_game_cap_refuses_games() {
        let cfg = Config {
            max_games: 0,
            ..Config::default()
        };
        let v = verdicts(&cfg, &facts(), &[], &[job("g", JobKind::Game, 1)]);
        assert!(matches!(
            v[0],
            Verdict::Refuse(RefusalReason::OverGameCap, _)
        ));
    }

    #[test]
    fn low_disk_refuses_builds_naming_the_path_but_not_games() {
        let cfg = Config {
            min_free_disk_gib: [("/data".to_string(), 50), ("/".to_string(), 20)].into(),
            ..Config::default()
        };
        let mut f = facts();
        f.free_disk[0].1 = 30 * GIB;
        let v = verdicts(
            &cfg,
            &f,
            &[],
            &[
                job("b", JobKind::Build, 1),
                job("h", JobKind::Harness, 1),
                job("g", JobKind::Game, 1),
            ],
        );
        for verdict in &v[..2] {
            match verdict {
                Verdict::Refuse(RefusalReason::DiskLow, d) => {
                    assert!(d.contains("/data") && d.contains("30"), "{d}")
                }
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(v[2], Verdict::Grant);
    }

    #[test]
    fn bench_waits_for_the_owner_to_leave_and_for_an_empty_machine() {
        let cfg = Config::default();
        let mut f = facts();
        f.owner_present = true;
        assert_eq!(
            wait_reason(&verdicts(&cfg, &f, &[], &[job("b", JobKind::Bench, 1)])[0]),
            Some(WaitingOn::OwnerPresent)
        );
        f.owner_present = false;
        assert_eq!(
            wait_reason(
                &verdicts(
                    &cfg,
                    &f,
                    &[run(JobKind::Build, 1)],
                    &[job("b", JobKind::Bench, 1)]
                )[0]
            ),
            Some(WaitingOn::Exclusive)
        );
        assert_eq!(
            verdicts(&cfg, &f, &[], &[job("b", JobKind::Bench, 1)]),
            [Verdict::Grant]
        );
    }

    #[test]
    fn nothing_is_admitted_while_a_bench_runs_or_is_granted_in_the_same_pass() {
        let cfg = Config::default();
        let f = facts();
        let v = verdicts(
            &cfg,
            &f,
            &[run(JobKind::Bench, 1)],
            &[job("t", JobKind::Test, 1)],
        );
        assert_eq!(wait_reason(&v[0]), Some(WaitingOn::Exclusive));
        let v = verdicts(
            &cfg,
            &f,
            &[],
            &[job("b", JobKind::Bench, 1), job("t", JobKind::Test, 1)],
        );
        assert_eq!(v[0], Verdict::Grant);
        assert_eq!(wait_reason(&v[1]), Some(WaitingOn::Exclusive));
    }

    #[test]
    fn a_bench_waiting_on_the_owner_never_blocks_the_line() {
        let cfg = Config::default();
        let mut f = facts();
        f.owner_present = true;
        f.now_ms = 3_600_000;
        let v = verdicts(
            &cfg,
            &f,
            &[],
            &[job("b", JobKind::Bench, 1), job("t", JobKind::Test, 1)],
        );
        assert_eq!(v[1], Verdict::Grant);
    }

    #[test]
    fn display_is_refused_while_the_owner_is_present() {
        let cfg = Config::default();
        let mut f = facts();
        let mut w = job("g", JobKind::Game, 1);
        w.needs_display = true;
        f.owner_present = true;
        assert!(matches!(
            verdicts(&cfg, &f, &[], &[w.clone()])[0],
            Verdict::Refuse(RefusalReason::DisplayForbidden, _)
        ));
        f.owner_present = false;
        assert_eq!(verdicts(&cfg, &f, &[], &[w]), [Verdict::Grant]);
    }

    #[test]
    fn unspecified_kind_is_refused() {
        let v = verdicts(
            &Config::default(),
            &facts(),
            &[],
            &[job("x", JobKind::Unspecified, 1)],
        );
        assert!(matches!(
            v[0],
            Verdict::Refuse(RefusalReason::UnknownKind, _)
        ));
    }

    #[test]
    fn cpu_is_capped_at_oversubscription_minus_unmanaged_load() {
        let cfg = Config::default();
        let mut f = facts();
        f.cpu_count = 8; // cap 12 cores
        f.unmanaged_cores = 4.0; // cap 8
        let mut builds = Vec::new();
        for i in 0..3 {
            let mut w = job(&format!("b{i}"), JobKind::Build, 1);
            w.expected_cores = 4.0;
            builds.push(w);
        }
        let v = verdicts(&cfg, &f, &[], &builds);
        assert_eq!(v[..2], [Verdict::Grant, Verdict::Grant]);
        assert_eq!(wait_reason(&v[2]), Some(WaitingOn::Cpu));
    }

    #[test]
    fn an_oversized_job_still_runs_on_an_idle_scheduler() {
        let cfg = Config::default();
        let mut f = facts();
        f.cpu_count = 2;
        let mut w = job("b", JobKind::Build, 1);
        w.expected_cores = 16.0;
        assert_eq!(verdicts(&cfg, &f, &[], &[w.clone()]), [Verdict::Grant]);
        assert_eq!(
            wait_reason(&verdicts(&cfg, &f, &[run(JobKind::Test, 1)], &[w])[0]),
            Some(WaitingOn::Cpu)
        );
    }

    #[test]
    fn same_kind_stays_fifo_when_blocked_on_cpu() {
        let cfg = Config::default();
        let mut f = facts();
        f.cpu_count = 4; // cap 6
        let mut first = job("first", JobKind::Build, 1);
        first.expected_cores = 6.0;
        let mut second = job("second", JobKind::Build, 1);
        second.expected_cores = 1.0;
        let v = verdicts(&cfg, &f, &[run(JobKind::Build, 1)], &[first, second]);
        assert_eq!(wait_reason(&v[0]), Some(WaitingOn::Cpu));
        assert_eq!(wait_reason(&v[1]), Some(WaitingOn::Cpu));
    }
}
