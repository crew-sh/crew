use std::collections::BTreeMap;

use crew_proto::{JobKind, RefusalReason, WaitingOn};

use crate::config::{Config, GIB};

#[derive(Clone, Debug)]
pub struct Facts {
    pub mem_total: u64,
    pub mem_available: u64,
    pub cpu_count: u32,
    pub unmanaged_cores: f64,
    pub free_disk: Vec<(String, u64)>,
    /// Watched paths that could not be measured; one with a threshold counts as low.
    pub unmeasurable_disk: Vec<String>,
    pub owner_present: bool,
    pub now_ms: u64,
}

#[derive(Clone, Debug)]
pub struct Running {
    /// How messages name this job, e.g. `bench 1a2b3c4d (horizon: ./m.sh)`.
    pub label: String,
    pub kind: JobKind,
    pub expected_mem: u64,
    pub expected_cores: f64,
    pub observed_peak: u64,
}

#[derive(Clone, Debug)]
pub struct Waiting {
    pub id: String,
    /// How messages name this job, e.g. `bench 1a2b3c4d (horizon: ./m.sh)`.
    pub label: String,
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

fn gib1(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / GIB as f64)
}

/// Another waiting job holds this one back, and why that one waits.
struct Blocker {
    label: String,
    kind: JobKind,
    reason: WaitingOn,
    detail: String,
    waited_ms: u64,
}

fn disk_problem(cfg: &Config, facts: &Facts) -> Option<String> {
    let unmeasurable = facts.unmeasurable_disk.iter().find_map(|path| {
        let min = cfg.min_free_disk(path)?;
        Some(format!(
            "{path} cannot be measured (is it mounted?), and it needs {} GiB free",
            gib(min)
        ))
    });
    unmeasurable.or_else(|| {
        facts.free_disk.iter().find_map(|(path, free)| {
            let min = cfg.min_free_disk(path)?;
            (*free < min).then(|| format!("{path} has {} GiB free, needs {}", gib(*free), gib(min)))
        })
    })
}

/// A waiting job competes with a starved one for what the starved one lacks.
fn competes(blocked_on: WaitingOn, blocker: JobKind, job: JobKind) -> bool {
    match blocked_on {
        WaitingOn::GameCap => blocker == job,
        _ => true,
    }
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
    if facts.mem_total > 0 && w.expected_mem + cfg.min_free_mem() > facts.mem_total {
        return Some((
            RefusalReason::TooLarge,
            format!(
                "needs {} + {} kept free = {}, but this machine has {} of memory; lower --mem or min_free_mem_gib",
                gib1(w.expected_mem),
                gib1(cfg.min_free_mem()),
                gib1(w.expected_mem + cfg.min_free_mem()),
                gib1(facts.mem_total),
            ),
        ));
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
    let mut exclusive_running = running
        .iter()
        .find(|r| cfg.kind(r.kind).exclusive)
        .map(|r| r.label.clone());
    let cap = (f64::from(facts.cpu_count) * cfg.cpu_oversub - facts.unmanaged_cores).max(0.0);
    let mut starved: Option<Blocker> = None;
    let mut kind_blocked: BTreeMap<i32, Blocker> = BTreeMap::new();
    let starve_ms = cfg.starve_after_secs * 1000;
    let mut out = Vec::with_capacity(queue.len());

    for w in queue {
        if let Some((reason, detail)) = refusal(cfg, facts, w) {
            out.push((w.id.clone(), Verdict::Refuse(reason, detail)));
            continue;
        }
        let spec = cfg.kind(w.kind);
        let free = facts.mem_available.saturating_sub(reserved);
        let wait = if let Some(b) = starved
            .as_ref()
            .filter(|b| competes(b.reason, b.kind, w.kind))
        {
            Some((
                b.reason,
                format!(
                    "queue order: behind {}, which has waited {}m on {}: {}",
                    b.label,
                    b.waited_ms / 60_000,
                    b.reason.name(),
                    b.detail
                ),
            ))
        } else if let Some(label) = &exclusive_running {
            Some((
                WaitingOn::Exclusive,
                format!("exclusive {label} is running"),
            ))
        } else if let Some(b) = kind_blocked.get(&(w.kind as i32)) {
            Some((
                b.reason,
                format!(
                    "queue order: behind {} (same kind), which waits on {}: {}",
                    b.label,
                    b.reason.name(),
                    b.detail
                ),
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
        } else if free < cfg.min_free_mem() + w.expected_mem {
            Some((
                WaitingOn::Memory,
                format!(
                    "needs {} + {} kept free = {}; {} available ({} free - {} reserved by other jobs); queue position is not the cause",
                    gib1(w.expected_mem),
                    gib1(cfg.min_free_mem()),
                    gib1(cfg.min_free_mem() + w.expected_mem),
                    gib1(free),
                    gib1(facts.mem_available),
                    gib1(reserved),
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
                if spec.exclusive {
                    exclusive_running = Some(w.label.clone());
                }
                out.push((w.id.clone(), Verdict::Grant));
            }
            Some((reason, detail)) => {
                let blocker = || Blocker {
                    label: w.label.clone(),
                    kind: w.kind,
                    reason,
                    detail: detail.clone(),
                    waited_ms: facts.now_ms.saturating_sub(w.queued_at_ms),
                };
                // Owner presence is not something a queue can wait out, so it never blocks the line.
                if reason != WaitingOn::OwnerPresent
                    && facts.now_ms.saturating_sub(w.queued_at_ms) >= starve_ms
                    && starved.is_none()
                {
                    starved = Some(blocker());
                }
                if reason != WaitingOn::Memory {
                    kind_blocked.entry(w.kind as i32).or_insert_with(blocker);
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
            mem_total: 128 * GIB,
            unmeasurable_disk: vec![],
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
            label: format!("job {id}"),
            kind,
            expected_mem: mem_gib * GIB,
            expected_cores: 1.0,
            needs_display: false,
            queued_at_ms: 0,
        }
    }

    fn run(kind: JobKind, mem_gib: u64) -> Running {
        Running {
            label: "running job".into(),
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
    fn a_job_that_can_never_fit_is_refused_with_the_numbers() {
        let cfg = Config::default();
        let mut f = facts();
        f.mem_total = 64 * GIB;
        // 64 total - 16 kept free = 48: a 48 GiB job fits the machine, a 49 GiB one never will.
        let v = verdicts(
            &cfg,
            &f,
            &[],
            &[
                job("fits", JobKind::Test, 48),
                job("never", JobKind::Test, 49),
            ],
        );
        assert_eq!(v[0], Verdict::Grant);
        match &v[1] {
            Verdict::Refuse(RefusalReason::TooLarge, d) => {
                assert!(d.contains("49.0 GiB") && d.contains("64.0 GiB"), "{d}")
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_starved_game_waiting_on_the_game_cap_does_not_block_other_kinds() {
        let cfg = Config {
            max_games: 1,
            ..Config::default()
        };
        let mut f = facts();
        f.now_ms = 11 * 60 * 1000;
        let v = verdicts(
            &cfg,
            &f,
            &[run(JobKind::Game, 1)],
            &[
                job("g", JobKind::Game, 1),
                job("t", JobKind::Test, 1),
                job("g2", JobKind::Game, 1),
            ],
        );
        assert_eq!(wait_reason(&v[0]), Some(WaitingOn::GameCap));
        assert_eq!(
            v[1],
            Verdict::Grant,
            "a test does not compete for the game cap"
        );
        assert_eq!(wait_reason(&v[2]), Some(WaitingOn::GameCap));
    }

    #[test]
    fn an_unmeasurable_watched_path_counts_as_low_disk() {
        let cfg = Config {
            min_free_disk_gib: [("/data".to_string(), 50)].into(),
            ..Config::default()
        };
        let mut f = facts();
        f.free_disk = vec![("/".into(), 100 * GIB)];
        f.unmeasurable_disk = vec!["/data".into()];
        let v = verdicts(
            &cfg,
            &f,
            &[],
            &[job("b", JobKind::Build, 1), job("g", JobKind::Game, 1)],
        );
        match &v[0] {
            Verdict::Refuse(RefusalReason::DiskLow, d) => assert!(
                d.contains("/data") && d.contains("cannot be measured"),
                "{d}"
            ),
            other => panic!("{other:?}"),
        }
        assert_eq!(v[1], Verdict::Grant, "games are not disk gated");
    }

    fn detail(v: &Verdict) -> &str {
        match v {
            Verdict::Wait(_, d) => d,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_memory_wait_states_the_rule_with_the_numbers() {
        let cfg = Config::default();
        let mut f = facts();
        f.mem_available = 30 * GIB;
        let running = Running {
            label: "test r".into(),
            kind: JobKind::Test,
            expected_mem: 2 * GIB,
            expected_cores: 1.0,
            observed_peak: 0,
        };
        let v = verdicts(&cfg, &f, &[running], &[job("big", JobKind::Test, 16)]);
        let d = detail(&v[0]);
        for part in [
            "16.0 GiB + 16.0 GiB kept free = 32.0 GiB",
            "28.0 GiB available",
            "30.0 GiB free - 2.0 GiB reserved",
            "queue position is not the cause",
        ] {
            assert!(d.contains(part), "{part} missing from {d}");
        }
    }

    #[test]
    fn a_job_held_by_a_starved_one_names_it_and_why_it_waits() {
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
        let d = detail(&v[1]);
        assert!(
            d.contains("job big") && d.contains("waited 11m on memory"),
            "{d}"
        );
        assert!(d.contains("20.0 GiB + 16.0 GiB kept free"), "{d}");
    }

    #[test]
    fn a_job_behind_a_running_exclusive_one_names_it() {
        let cfg = Config::default();
        let running = Running {
            label: "bench abc".into(),
            ..run(JobKind::Bench, 1)
        };
        let v = verdicts(&cfg, &facts(), &[running], &[job("t", JobKind::Test, 1)]);
        assert!(detail(&v[0]).contains("bench abc"), "{}", detail(&v[0]));
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
