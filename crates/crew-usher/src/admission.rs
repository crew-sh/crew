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
    /// Since when the owner has been away (unix ms); `None` while present.
    pub owner_away_since_ms: Option<u64>,
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
    /// The job asked to run alone (`--exclusive`); its kind's `exclusive` setting counts too.
    pub exclusive: bool,
    /// When it should end, from learned durations; `None` when that is unknown.
    pub ends_at_ms: Option<u64>,
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
    /// The job asked to run alone (`--exclusive`); its kind's `exclusive` setting counts too.
    pub exclusive: bool,
    /// How long it should run, from learned durations; `None` when that is unknown.
    pub expected_wall_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Grant,
    Refuse(RefusalReason, String),
    Wait(WaitingOn, String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Schedule {
    /// One verdict per queue entry, in queue order.
    pub verdicts: Vec<(String, Verdict)>,
    /// The exclusive job the machine drains for: it waited long enough, so no new job starts before it.
    pub draining_for: Option<String>,
}

/// The job runs with nothing beside it.
pub fn runs_alone(cfg: &Config, kind: JobKind, asked: bool) -> bool {
    asked || cfg.kind(kind).exclusive
}

/// The job runs only while the owner is away.
pub fn away_only(cfg: &Config, kind: JobKind, asked: bool) -> bool {
    runs_alone(cfg, kind, asked) || cfg.kind(kind).away_only
}

fn disk_gated(kind: JobKind) -> bool {
    matches!(kind, JobKind::Build | JobKind::Test | JobKind::Harness)
}

fn gib(bytes: u64) -> u64 {
    bytes / GIB
}

pub(crate) fn gib1(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / GIB as f64)
}

fn minutes(ms: u64) -> u64 {
    ms / 60_000
}

/// What a running or just-granted job holds, and when it gives it back.
struct Held {
    /// Free memory comes back by this much when it ends: its reservation plus what it uses.
    mem: u64,
    cores: f64,
    ends_at_ms: Option<u64>,
}

/// When a starved job can start if nothing new takes what it needs, and what it leaves beside it then.
struct Shadow {
    /// `None` when a job it waits for has no learned duration.
    at_ms: Option<u64>,
    spare_mem: u64,
    spare_cores: f64,
}

/// A job that waited `starve_after_secs` on memory or cpu: later jobs start only where they do not delay it.
struct Hold {
    label: String,
    reason: WaitingOn,
    detail: String,
    waited_ms: u64,
    shadow: Shadow,
}

impl Hold {
    /// Lets `w` start if it ends before the held job can start, or fits in what that job leaves spare.
    fn admits(&mut self, w: &Waiting, now_ms: u64) -> Result<(), String> {
        let sh = &mut self.shadow;
        if let (Some(at), Some(wall)) = (sh.at_ms, w.expected_wall_ms)
            && now_ms + wall <= at
        {
            return Ok(());
        }
        if w.expected_mem <= sh.spare_mem && w.expected_cores <= sh.spare_cores {
            sh.spare_mem -= w.expected_mem;
            sh.spare_cores -= w.expected_cores;
            return Ok(());
        }
        let when = match sh.at_ms {
            Some(at) => format!(
                "it can start in {}m, before this job would end",
                minutes(at.saturating_sub(now_ms))
            ),
            None => "it starts when jobs with no learned duration end".to_string(),
        };
        Err(format!(
            "{} is held for {}, which has waited {}m on {}: {}; {when}, and this job needs more than the {} and {:.1} cores it leaves spare",
            self.reason.name(),
            self.label,
            minutes(self.waited_ms),
            self.reason.name(),
            self.detail,
            gib1(sh.spare_mem),
            sh.spare_cores,
        ))
    }
}

/// Walks the held jobs' ends in order until `w` fits; what is spare then is what later jobs may use.
fn shadow(
    cfg: &Config,
    now_ms: u64,
    cap: f64,
    free: u64,
    cores: f64,
    held: &[Held],
    w: &Waiting,
) -> Shadow {
    let need = cfg.min_free_mem() + w.expected_mem;
    let fits = |free: u64, cores: f64, left: usize| {
        free >= need && (left == 0 || cores + w.expected_cores <= cap)
    };
    let mut ends: Vec<&Held> = held.iter().collect();
    ends.sort_by_key(|h| h.ends_at_ms.unwrap_or(u64::MAX));
    let (mut free, mut cores, mut left) = (free, cores, held.len());
    let mut at = Some(now_ms);
    for h in ends {
        if fits(free, cores, left) {
            break;
        }
        at = at.zip(h.ends_at_ms).map(|(a, e)| a.max(e));
        free += h.mem;
        cores -= h.cores;
        left -= 1;
    }
    if !fits(free, cores, left) {
        at = None;
    }
    Shadow {
        at_ms: at,
        spare_mem: free.saturating_sub(need),
        spare_cores: (cap - cores.max(0.0) - w.expected_cores).max(0.0),
    }
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

/// The oldest exclusive job, once it has waited `exclusive_drain_after_secs` while the owner was away.
fn drain_target<'a>(
    cfg: &Config,
    facts: &Facts,
    queue: &'a [Waiting],
    alone_running: bool,
) -> Option<&'a Waiting> {
    if facts.owner_present || alone_running {
        return None;
    }
    let away_since = facts.owner_away_since_ms.unwrap_or(facts.now_ms);
    let first = queue
        .iter()
        .find(|w| runs_alone(cfg, w.kind, w.exclusive) && refusal(cfg, facts, w).is_none())?;
    let waited = facts
        .now_ms
        .saturating_sub(first.queued_at_ms.max(away_since));
    (waited >= cfg.exclusive_drain_after_secs * 1000).then_some(first)
}

/// One verdict per queue entry, in queue order; grants consume the capacity later entries see.
pub fn schedule(cfg: &Config, facts: &Facts, running: &[Running], queue: &[Waiting]) -> Schedule {
    let now = facts.now_ms;
    let mut reserved: u64 = running
        .iter()
        .map(|r| r.expected_mem.saturating_sub(r.observed_peak))
        .sum();
    let mut cores: f64 = running.iter().map(|r| r.expected_cores).sum();
    let mut games = running.iter().filter(|r| r.kind == JobKind::Game).count() as u32;
    let mut held: Vec<Held> = running
        .iter()
        .map(|r| Held {
            mem: r.expected_mem.max(r.observed_peak),
            cores: r.expected_cores,
            ends_at_ms: r.ends_at_ms,
        })
        .collect();
    let mut alone = running
        .iter()
        .find(|r| runs_alone(cfg, r.kind, r.exclusive))
        .map(|r| r.label.clone());
    let cap = (f64::from(facts.cpu_count) * cfg.cpu_oversub - facts.unmanaged_cores).max(0.0);
    let drain = drain_target(cfg, facts, queue, alone.is_some());
    let mut hold: Option<Hold> = None;
    let starve_ms = cfg.starve_after_secs * 1000;
    let mut verdicts = Vec::with_capacity(queue.len());

    for w in queue {
        if let Some((reason, detail)) = refusal(cfg, facts, w) {
            verdicts.push((w.id.clone(), Verdict::Refuse(reason, detail)));
            continue;
        }
        let alone_w = runs_alone(cfg, w.kind, w.exclusive);
        let free = facts.mem_available.saturating_sub(reserved);
        let active = held.len();
        let wait = if let Some(label) = &alone {
            Some((
                WaitingOn::Exclusive,
                format!("exclusive {label} is running"),
            ))
        } else if away_only(cfg, w.kind, w.exclusive) && facts.owner_present {
            let what = if alone_w { "exclusive" } else { w.kind.name() };
            Some((
                WaitingOn::OwnerPresent,
                format!("{what} jobs run only while the owner is away"),
            ))
        } else if let Some(d) = drain.filter(|d| d.id != w.id) {
            Some((
                WaitingOn::Drain,
                format!(
                    "draining for {}, which has waited {}m to run alone; nothing new starts until it has run",
                    d.label,
                    minutes(now.saturating_sub(
                        d.queued_at_ms.max(facts.owner_away_since_ms.unwrap_or(now))
                    )),
                ),
            ))
        } else if alone_w && active > 0 {
            Some((
                WaitingOn::Exclusive,
                format!(
                    "{active} other job(s) running; others may start beside them until it has waited {}m, then the machine drains for it",
                    minutes(cfg.exclusive_drain_after_secs * 1000)
                ),
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
        } else if let Some(h) = hold.as_mut() {
            h.admits(w, now).err().map(|why| (h.reason, why))
        } else {
            None
        };

        match wait {
            None => {
                reserved += w.expected_mem;
                cores += w.expected_cores;
                games += u32::from(w.kind == JobKind::Game);
                held.push(Held {
                    mem: w.expected_mem,
                    cores: w.expected_cores,
                    ends_at_ms: w.expected_wall_ms.map(|d| now + d),
                });
                if alone_w {
                    alone = Some(w.label.clone());
                }
                verdicts.push((w.id.clone(), Verdict::Grant));
            }
            Some((reason, detail)) => {
                let waited_ms = now.saturating_sub(w.queued_at_ms);
                // Only waits on memory or cpu starve: owner presence and exclusivity have their own rules.
                if hold.is_none()
                    && matches!(reason, WaitingOn::Memory | WaitingOn::Cpu)
                    && waited_ms >= starve_ms
                {
                    hold = Some(Hold {
                        label: w.label.clone(),
                        reason,
                        detail: detail.clone(),
                        waited_ms,
                        shadow: shadow(cfg, now, cap, free, cores, &held, w),
                    });
                }
                verdicts.push((w.id.clone(), Verdict::Wait(reason, detail)));
            }
        }
    }
    let draining_for = drain
        .filter(|d| {
            verdicts
                .iter()
                .any(|(id, v)| *id == d.id && matches!(v, Verdict::Wait(..)))
        })
        .map(|d| d.label.clone());
    Schedule {
        verdicts,
        draining_for,
    }
}

/// How long a killed job's memory gets to come back after its scope ends before the usher may kill again.
pub const KILL_SETTLE_MS: u64 = 5_000;
/// A job killed for memory this many times runs alone from then on.
pub const ALONE_AFTER_KILLS: u32 = 2;

/// Where the last memory kill stands: one kill at a time, and none until its memory is back.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum KillGate {
    #[default]
    Open,
    /// The killed job's scope still exists.
    Dying { scope: String },
    /// The scope ended at this time.
    Settling { since_ms: u64 },
}

impl KillGate {
    /// Moves on once the killed scope is gone.
    pub fn advance(&mut self, scope_alive: impl Fn(&str) -> bool, now_ms: u64) {
        if let Self::Dying { scope } = self
            && !scope_alive(scope)
        {
            *self = Self::Settling { since_ms: now_ms };
        }
    }

    fn open(&self, now_ms: u64) -> bool {
        match self {
            Self::Open => true,
            Self::Dying { .. } => false,
            Self::Settling { since_ms } => now_ms.saturating_sub(*since_ms) >= KILL_SETTLE_MS,
        }
    }
}

/// A running job as the memory kill sees it.
#[derive(Clone, Debug)]
pub struct Killable {
    pub id: String,
    pub kind: JobKind,
    /// The job asked to run alone (`--exclusive`), or runs alone after earlier kills.
    pub exclusive: bool,
    pub started_ms: u64,
    /// Its scope has been seen; a job still starting holds nothing a kill would free.
    pub has_scope: bool,
    /// Already on its way out: killed before, or cancelled.
    pub ending: bool,
}

/// The job to kill when available memory is under `kill_below_free_mem_gib`: the newest running one of a restartable kind that does not run alone.
pub fn memory_victim<'a>(
    cfg: &Config,
    mem_available: u64,
    now_ms: u64,
    gate: &KillGate,
    running: &'a [Killable],
) -> Option<&'a Killable> {
    if mem_available >= cfg.kill_below_free_mem() || !gate.open(now_ms) {
        return None;
    }
    running
        .iter()
        .filter(|k| {
            k.has_scope
                && !k.ending
                && cfg.kind(k.kind).restartable
                && !runs_alone(cfg, k.kind, k.exclusive)
        })
        .max_by_key(|k| k.started_ms)
}

/// What a job killed for memory reserves next: 1.5 times the peak it reached, never less than before, never more than the machine admits.
pub fn retry_mem(previous: u64, peak: u64, ceiling: u64) -> u64 {
    previous
        .max(peak.saturating_mul(3) / 2)
        .min(ceiling.max(previous))
}

/// The part of a retried job's wait detail that says why it is back in the queue.
pub fn retry_note(kills: u32, peak: u64, expected_mem: u64) -> String {
    let alone = if kills >= ALONE_AFTER_KILLS {
        ", alone"
    } else {
        ""
    };
    format!(
        "killed {kills}x for memory (peak {}); reruns with {} reserved{alone}",
        gib1(peak),
        gib1(expected_mem)
    )
}

/// When each waiting job should start, in ms from now, by replaying admission as running jobs end at their learned durations.
pub fn starts(
    cfg: &Config,
    facts: &Facts,
    running: &[Running],
    queue: &[Waiting],
) -> BTreeMap<String, u64> {
    const MAX_STEPS: usize = 256;
    let (mut facts, mut running, mut queue) = (facts.clone(), running.to_vec(), queue.to_vec());
    let begin = facts.now_ms;
    let mut out = BTreeMap::new();
    for _ in 0..MAX_STEPS {
        let now = facts.now_ms;
        for (id, verdict) in schedule(cfg, &facts, &running, &queue).verdicts {
            let Some(at) = queue.iter().position(|w| w.id == id) else {
                continue;
            };
            match verdict {
                Verdict::Wait(..) => {}
                Verdict::Refuse(..) => {
                    queue.remove(at);
                }
                Verdict::Grant => {
                    let w = queue.remove(at);
                    running.push(Running {
                        label: w.label,
                        kind: w.kind,
                        expected_mem: w.expected_mem,
                        expected_cores: w.expected_cores,
                        observed_peak: 0,
                        exclusive: w.exclusive,
                        ends_at_ms: w.expected_wall_ms.map(|d| now + d),
                    });
                    out.insert(id, now - begin);
                }
            }
        }
        if queue.is_empty() {
            break;
        }
        let Some((end, i)) = running
            .iter()
            .enumerate()
            .filter_map(|(i, r)| Some((r.ends_at_ms?.max(now), i)))
            .min()
        else {
            break;
        };
        let ended = running.remove(i);
        // What it used is free again; its reservation leaves with it.
        facts.mem_available += ended.observed_peak;
        facts.now_ms = end;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: u64 = 60_000;

    fn facts() -> Facts {
        Facts {
            mem_total: 128 * GIB,
            unmeasurable_disk: vec![],
            mem_available: 64 * GIB,
            cpu_count: 24,
            unmanaged_cores: 0.0,
            free_disk: vec![("/data".into(), 500 * GIB), ("/".into(), 100 * GIB)],
            owner_present: false,
            owner_away_since_ms: Some(0),
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
            exclusive: false,
            expected_wall_ms: None,
        }
    }

    fn exclusive(id: &str) -> Waiting {
        Waiting {
            exclusive: true,
            ..job(id, JobKind::Bench, 1)
        }
    }

    fn run(kind: JobKind, mem_gib: u64) -> Running {
        Running {
            label: "running job".into(),
            kind,
            expected_mem: mem_gib * GIB,
            expected_cores: 1.0,
            observed_peak: 0,
            exclusive: false,
            ends_at_ms: None,
        }
    }

    fn verdicts(cfg: &Config, f: &Facts, running: &[Running], queue: &[Waiting]) -> Vec<Verdict> {
        schedule(cfg, f, running, queue)
            .verdicts
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

    fn detail(v: &Verdict) -> &str {
        match v {
            Verdict::Wait(_, d) => d,
            other => panic!("{other:?}"),
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
    fn a_starved_job_holds_the_memory_it_lacks() {
        let cfg = Config::default();
        let mut f = facts();
        f.mem_available = 30 * GIB;
        f.now_ms = 11 * MIN;
        let q = [
            job("big", JobKind::Test, 20),
            job("small", JobKind::Test, 2),
        ];
        let v = verdicts(&cfg, &f, &[], &q);
        assert_eq!(wait_reason(&v[0]), Some(WaitingOn::Memory));
        assert_eq!(wait_reason(&v[1]), Some(WaitingOn::Memory));
        f.now_ms = 9 * MIN;
        assert_eq!(verdicts(&cfg, &f, &[], &q)[1], Verdict::Grant);
    }

    #[test]
    fn a_job_held_by_a_starved_one_names_it_and_why_it_waits() {
        let cfg = Config::default();
        let mut f = facts();
        f.mem_available = 30 * GIB;
        f.now_ms = 11 * MIN;
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
            d.contains("memory is held for job big") && d.contains("waited 11m on memory"),
            "{d}"
        );
        assert!(d.contains("20.0 GiB + 16.0 GiB kept free"), "{d}");
    }

    #[test]
    fn a_starved_job_lets_through_what_it_leaves_spare_or_what_ends_before_it_can_start() {
        let cfg = Config::default();
        let mut f = facts();
        // 40 available, of which a running build reserves 20 and gives them back at minute 30.
        f.mem_available = 40 * GIB;
        f.now_ms = 11 * MIN;
        let build = Running {
            ends_at_ms: Some(30 * MIN),
            ..run(JobKind::Build, 20)
        };
        // At minute 30: 20 + 20 = 40 free, big takes 24 + 16, nothing spare.
        let big = job("big", JobKind::Test, 24);
        let short = Waiting {
            expected_wall_ms: Some(10 * MIN),
            ..job("short", JobKind::Test, 2)
        };
        let long = Waiting {
            expected_wall_ms: Some(60 * MIN),
            ..job("long", JobKind::Test, 2)
        };
        let unknown = job("unknown", JobKind::Test, 2);
        let v = verdicts(
            &cfg,
            &f,
            std::slice::from_ref(&build),
            &[big.clone(), short, long, unknown],
        );
        assert_eq!(wait_reason(&v[0]), Some(WaitingOn::Memory));
        assert_eq!(v[1], Verdict::Grant, "it ends before big can start");
        assert_eq!(wait_reason(&v[2]), Some(WaitingOn::Memory));
        assert!(
            detail(&v[2]).contains("can start in 19m"),
            "{}",
            detail(&v[2])
        );
        assert_eq!(wait_reason(&v[3]), Some(WaitingOn::Memory));

        // With 8 GiB more on the machine, 8 is spare beside big at minute 30: a 2 GiB job of any length fits.
        f.mem_available = 48 * GIB;
        let unknown = job("unknown", JobKind::Test, 2);
        let v = verdicts(&cfg, &f, &[build], &[big, unknown]);
        assert_eq!(v[1], Verdict::Grant);
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
        f.now_ms = 11 * MIN;
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

    #[test]
    fn a_memory_wait_states_the_rule_with_the_numbers() {
        let cfg = Config::default();
        let mut f = facts();
        f.mem_available = 30 * GIB;
        let running = Running {
            label: "test r".into(),
            expected_mem: 2 * GIB,
            ..run(JobKind::Test, 2)
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
    fn a_job_behind_a_running_exclusive_one_names_it() {
        let cfg = Config::default();
        let running = Running {
            label: "bench abc".into(),
            exclusive: true,
            ..run(JobKind::Bench, 1)
        };
        let v = verdicts(&cfg, &facts(), &[running], &[job("t", JobKind::Test, 1)]);
        assert_eq!(wait_reason(&v[0]), Some(WaitingOn::Exclusive));
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
    fn a_bench_waits_for_the_owner_to_leave_but_not_for_other_jobs() {
        let cfg = Config::default();
        let mut f = facts();
        f.owner_present = true;
        f.owner_away_since_ms = None;
        assert_eq!(
            wait_reason(&verdicts(&cfg, &f, &[], &[job("b", JobKind::Bench, 1)])[0]),
            Some(WaitingOn::OwnerPresent)
        );
        f.owner_present = false;
        f.owner_away_since_ms = Some(0);
        assert_eq!(
            verdicts(
                &cfg,
                &f,
                &[run(JobKind::Build, 1)],
                &[job("b", JobKind::Bench, 1)]
            ),
            [Verdict::Grant]
        );
    }

    #[test]
    fn a_build_with_memory_free_never_waits_behind_a_bench() {
        let cfg = Config::default();
        // The 2026-11-12 machine: 62 GiB, a 20 GiB bench running (5 used so far), another queued for hours.
        let mut f = facts();
        f.mem_total = 62 * GIB;
        f.mem_available = 45 * GIB;
        f.now_ms = 120 * MIN;
        let running = Running {
            observed_peak: 5 * GIB,
            ..run(JobKind::Bench, 20)
        };
        let bench = job("bench", JobKind::Bench, 20);
        let build = Waiting {
            queued_at_ms: 100 * MIN,
            ..job("build", JobKind::Build, 12)
        };
        let v = verdicts(
            &cfg,
            &f,
            std::slice::from_ref(&running),
            &[bench.clone(), build.clone()],
        );
        assert_eq!(wait_reason(&v[0]), Some(WaitingOn::Memory));
        assert_eq!(v[1], Verdict::Grant);
        // Nor while the owner is here and the bench cannot start at all.
        f.owner_present = true;
        f.owner_away_since_ms = None;
        let v = verdicts(&cfg, &f, &[running], &[bench, build]);
        assert_eq!(wait_reason(&v[0]), Some(WaitingOn::OwnerPresent));
        assert_eq!(v[1], Verdict::Grant);
    }

    #[test]
    fn two_benches_that_fit_run_together_and_beside_builds() {
        let cfg = Config::default();
        let v = verdicts(
            &cfg,
            &facts(),
            &[run(JobKind::Build, 4)],
            &[
                job("b1", JobKind::Bench, 8),
                job("b2", JobKind::Bench, 8),
                job("t", JobKind::Test, 2),
            ],
        );
        assert_eq!(v, [Verdict::Grant, Verdict::Grant, Verdict::Grant]);
    }

    #[test]
    fn an_exclusive_job_waits_for_the_owner_and_runs_alone() {
        let cfg = Config::default();
        let mut f = facts();
        f.owner_present = true;
        f.owner_away_since_ms = None;
        assert_eq!(
            wait_reason(&verdicts(&cfg, &f, &[], &[exclusive("e")])[0]),
            Some(WaitingOn::OwnerPresent)
        );
        f.owner_present = false;
        f.owner_away_since_ms = Some(0);
        let v = verdicts(&cfg, &f, &[], &[exclusive("e"), job("t", JobKind::Test, 1)]);
        assert_eq!(v[0], Verdict::Grant);
        assert_eq!(wait_reason(&v[1]), Some(WaitingOn::Exclusive));
    }

    #[test]
    fn the_kind_setting_still_makes_every_job_of_a_kind_exclusive() {
        let cfg: Config = toml::from_str("[kinds.bench]\nexclusive = true\n").unwrap();
        let v = verdicts(
            &cfg,
            &facts(),
            &[run(JobKind::Build, 1)],
            &[job("b", JobKind::Bench, 1)],
        );
        assert_eq!(wait_reason(&v[0]), Some(WaitingOn::Exclusive));
    }

    #[test]
    fn an_exclusive_job_drains_after_the_threshold_and_then_runs_alone() {
        let cfg = Config::default();
        let mut f = facts();
        let build = run(JobKind::Build, 1);
        let later = Waiting {
            queued_at_ms: MIN,
            ..job("t", JobKind::Test, 1)
        };
        let q = [exclusive("e"), later];

        f.now_ms = 5 * MIN;
        let s = schedule(&cfg, &f, std::slice::from_ref(&build), &q);
        assert_eq!(wait_reason(&s.verdicts[0].1), Some(WaitingOn::Exclusive));
        assert_eq!(
            s.verdicts[1].1,
            Verdict::Grant,
            "backfill while it cannot start"
        );
        assert_eq!(s.draining_for, None);

        f.now_ms = 11 * MIN;
        let s = schedule(&cfg, &f, std::slice::from_ref(&build), &q);
        assert_eq!(wait_reason(&s.verdicts[0].1), Some(WaitingOn::Exclusive));
        assert_eq!(wait_reason(&s.verdicts[1].1), Some(WaitingOn::Drain));
        assert!(detail(&s.verdicts[1].1).contains("draining for job e"));
        assert_eq!(s.draining_for.as_deref(), Some("job e"));

        let s = schedule(&cfg, &f, &[], &q);
        assert_eq!(s.verdicts[0].1, Verdict::Grant);
        assert_eq!(wait_reason(&s.verdicts[1].1), Some(WaitingOn::Exclusive));
    }

    #[test]
    fn the_drain_clock_runs_only_while_the_owner_is_away() {
        let cfg = Config::default();
        let mut f = facts();
        let q = [exclusive("e"), job("t", JobKind::Test, 1)];
        let running = [run(JobKind::Build, 1)];
        f.owner_away_since_ms = Some(20 * MIN);
        f.now_ms = 25 * MIN;
        assert_eq!(verdicts(&cfg, &f, &running, &q)[1], Verdict::Grant);
        f.now_ms = 31 * MIN;
        assert_eq!(
            wait_reason(&verdicts(&cfg, &f, &running, &q)[1]),
            Some(WaitingOn::Drain)
        );
        f.owner_present = true;
        f.owner_away_since_ms = None;
        let s = schedule(&cfg, &f, &running, &q);
        assert_eq!(s.verdicts[1].1, Verdict::Grant);
        assert_eq!(s.draining_for, None);
    }

    /// Minute by minute: jobs of up to `longest` minutes arrive all the time; the exclusive one must still start.
    #[test]
    fn backfill_never_delays_an_exclusive_job_beyond_drain_plus_the_longest_running_job() {
        let cfg = Config::default();
        let longest = 45;
        let mut running = vec![(run(JobKind::Build, 1), 20 * MIN)];
        let mut queue = vec![exclusive("e")];
        let mut started = None;
        for minute in 0..200u64 {
            let now = minute * MIN;
            running.retain(|(_, end)| *end > now);
            let id = format!("j{minute}");
            queue.push(Waiting {
                queued_at_ms: now,
                ..job(&id, JobKind::Test, 1)
            });
            let f = Facts {
                now_ms: now,
                ..facts()
            };
            let r: Vec<Running> = running.iter().map(|(r, _)| r.clone()).collect();
            let v = schedule(&cfg, &f, &r, &queue).verdicts;
            for (id, verdict) in v {
                if verdict != Verdict::Grant {
                    continue;
                }
                let at = queue.iter().position(|w| w.id == id).unwrap();
                let w = queue.remove(at);
                let len = if w.exclusive {
                    5
                } else {
                    5 + (minute * 7) % (longest - 4)
                };
                if w.exclusive {
                    assert!(running.is_empty(), "the exclusive job starts alone");
                    started = Some(minute);
                }
                running.push((
                    Running {
                        label: w.label,
                        kind: w.kind,
                        exclusive: w.exclusive,
                        ..run(JobKind::Test, 1)
                    },
                    now + len * MIN,
                ));
            }
            if started.is_some() {
                break;
            }
        }
        let drain = cfg.exclusive_drain_after_secs / 60;
        let started = started.expect("the exclusive job started");
        assert!(
            started >= drain,
            "it was blocked by backfill until the drain"
        );
        assert!(started <= drain + longest, "started at minute {started}");
    }

    #[test]
    fn a_bench_waiting_on_the_owner_never_blocks_the_line() {
        let cfg = Config::default();
        let mut f = facts();
        f.owner_present = true;
        f.owner_away_since_ms = None;
        f.now_ms = 3_600_000;
        let v = verdicts(&cfg, &f, &[], &[exclusive("b"), job("t", JobKind::Test, 1)]);
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
    fn a_smaller_job_of_the_same_kind_passes_one_waiting_on_cpu_until_it_starves() {
        let cfg = Config::default();
        let mut f = facts();
        f.cpu_count = 4; // cap 6
        let first = Waiting {
            expected_cores: 6.0,
            ..job("first", JobKind::Build, 1)
        };
        let second = job("second", JobKind::Build, 1);
        let q = [first, second];
        let running = [run(JobKind::Build, 1)];
        let v = verdicts(&cfg, &f, &running, &q);
        assert_eq!(wait_reason(&v[0]), Some(WaitingOn::Cpu));
        assert_eq!(v[1], Verdict::Grant);
        f.now_ms = 11 * MIN;
        let v = verdicts(&cfg, &f, &running, &q);
        assert_eq!(wait_reason(&v[1]), Some(WaitingOn::Cpu));
        assert!(
            detail(&v[1]).contains("cpu is held for job first"),
            "{}",
            detail(&v[1])
        );
    }

    fn killable(id: &str, kind: JobKind, started_ms: u64) -> Killable {
        Killable {
            id: id.into(),
            kind,
            exclusive: false,
            started_ms,
            has_scope: true,
            ending: false,
        }
    }

    fn victim(
        cfg: &Config,
        avail_gib: u64,
        gate: &KillGate,
        running: &[Killable],
    ) -> Option<String> {
        memory_victim(cfg, avail_gib * GIB, 100 * MIN, gate, running).map(|k| k.id.clone())
    }

    #[test]
    fn low_memory_kills_the_newest_restartable_job_and_nothing_else() {
        let cfg = Config::default();
        let running = [
            killable("old-build", JobKind::Build, MIN),
            killable("new-test", JobKind::Test, 3 * MIN),
            killable("newer-harness", JobKind::Harness, 4 * MIN),
            killable("newest-game", JobKind::Game, 5 * MIN),
            Killable {
                exclusive: true,
                ..killable("newest-exclusive-build", JobKind::Build, 6 * MIN)
            },
        ];
        assert_eq!(
            victim(&cfg, 2, &KillGate::Open, &running).as_deref(),
            Some("new-test")
        );
        assert_eq!(
            victim(&cfg, 3, &KillGate::Open, &running),
            None,
            "3 GiB is not below 3"
        );
        assert_eq!(victim(&cfg, 40, &KillGate::Open, &running), None);
        assert_eq!(
            victim(&cfg, 2, &KillGate::Open, &running[2..]),
            None,
            "harness, game and exclusive jobs are never killed"
        );
    }

    #[test]
    fn a_job_still_starting_or_already_ending_is_passed_over() {
        let cfg = Config::default();
        let running = [
            killable("older", JobKind::Build, MIN),
            Killable {
                ending: true,
                ..killable("killed-before", JobKind::Build, 2 * MIN)
            },
            Killable {
                has_scope: false,
                ..killable("starting", JobKind::Build, 3 * MIN)
            },
        ];
        assert_eq!(
            victim(&cfg, 1, &KillGate::Open, &running).as_deref(),
            Some("older")
        );
    }

    #[test]
    fn a_kind_made_restartable_or_not_in_the_file_is_obeyed() {
        let cfg: Config = toml::from_str(
            "[kinds.test]\nrestartable = false\n[kinds.harness]\nrestartable = true\n",
        )
        .unwrap();
        let running = [
            killable("h", JobKind::Harness, MIN),
            killable("t", JobKind::Test, 2 * MIN),
        ];
        assert_eq!(
            victim(&cfg, 1, &KillGate::Open, &running).as_deref(),
            Some("h")
        );
        let exclusive_kind: Config = toml::from_str("[kinds.build]\nexclusive = true\n").unwrap();
        assert_eq!(
            victim(
                &exclusive_kind,
                1,
                &KillGate::Open,
                &[killable("b", JobKind::Build, MIN)]
            ),
            None
        );
    }

    #[test]
    fn zero_never_kills() {
        let cfg = Config {
            kill_below_free_mem_gib: 0,
            ..Config::default()
        };
        assert_eq!(
            victim(
                &cfg,
                0,
                &KillGate::Open,
                &[killable("b", JobKind::Build, MIN)]
            ),
            None
        );
    }

    #[test]
    fn one_kill_at_a_time_and_none_until_its_memory_had_time_to_come_back() {
        let cfg = Config::default();
        let running = [killable("b", JobKind::Build, MIN)];
        let now = 100 * MIN;
        let mut gate = KillGate::Dying {
            scope: "crew-build-x".into(),
        };
        assert_eq!(
            victim(&cfg, 1, &gate, &running),
            None,
            "its scope is still there"
        );
        gate.advance(|_| true, now);
        assert_eq!(victim(&cfg, 1, &gate, &running), None);
        gate.advance(|s| s != "crew-build-x", now - KILL_SETTLE_MS + 1);
        assert!(matches!(gate, KillGate::Settling { .. }));
        assert_eq!(victim(&cfg, 1, &gate, &running), None, "still settling");
        gate.advance(|_| true, now);
        assert!(
            matches!(gate, KillGate::Settling { .. }),
            "settling does not go back to dying"
        );
        let settled = KillGate::Settling {
            since_ms: now - KILL_SETTLE_MS,
        };
        assert_eq!(victim(&cfg, 1, &settled, &running).as_deref(), Some("b"));
    }

    #[test]
    fn a_retry_reserves_half_again_its_peak_but_never_less_than_before() {
        let ceiling = 46 * GIB;
        assert_eq!(retry_mem(2 * GIB, 10 * GIB, ceiling), 15 * GIB);
        assert_eq!(
            retry_mem(20 * GIB, 10 * GIB, ceiling),
            20 * GIB,
            "a larger earlier figure, from --mem or a learned size, stays"
        );
        assert_eq!(
            retry_mem(4 * GIB, 40 * GIB, ceiling),
            46 * GIB,
            "no more than the machine admits, so it is not refused as too large"
        );
        assert_eq!(retry_mem(50 * GIB, 60 * GIB, ceiling), 50 * GIB);
    }

    #[test]
    fn a_retry_note_names_the_kills_the_peak_and_the_new_size() {
        let note = retry_note(1, 10 * GIB, 15 * GIB);
        assert!(
            note.contains("1x") && note.contains("10.0 GiB") && note.contains("15.0 GiB"),
            "{note}"
        );
        assert!(!note.contains("alone"), "{note}");
        assert!(retry_note(ALONE_AFTER_KILLS, GIB, GIB).contains("alone"));
    }

    #[test]
    fn start_estimates_follow_learned_durations() {
        let cfg = Config::default();
        let mut f = facts();
        f.mem_available = 40 * GIB;
        let build = Running {
            observed_peak: 20 * GIB,
            ends_at_ms: Some(10 * MIN),
            ..run(JobKind::Build, 20)
        };
        // 40 free now; t needs 30 + 16 and fits once the build's 20 come back.
        let t = Waiting {
            expected_wall_ms: Some(30 * MIN),
            ..job("t", JobKind::Test, 30)
        };
        let after = job("after", JobKind::Test, 30);
        let eta = starts(
            &cfg,
            &f,
            std::slice::from_ref(&build),
            &[t.clone(), after.clone()],
        );
        assert_eq!(eta.get("t"), Some(&(10 * MIN)));
        assert_eq!(eta.get("after"), Some(&(40 * MIN)));

        let unknown = Running {
            ends_at_ms: None,
            ..build
        };
        assert!(starts(&cfg, &f, &[unknown], &[t, after]).is_empty());
    }

    #[test]
    fn an_exclusive_job_is_expected_when_the_running_jobs_end() {
        let cfg = Config::default();
        let f = facts();
        let build = Running {
            ends_at_ms: Some(60 * MIN),
            ..run(JobKind::Build, 1)
        };
        let e = Waiting {
            expected_wall_ms: Some(5 * MIN),
            ..exclusive("e")
        };
        let eta = starts(&cfg, &f, &[build], &[e]);
        assert_eq!(eta.get("e"), Some(&(60 * MIN)));
    }
}
