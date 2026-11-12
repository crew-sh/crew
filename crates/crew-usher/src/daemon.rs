use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crew_proto::{
    Ack, Decision, DiskView, Envelope, Grant, JobDone, JobKind, JobRequest, JobView, Limits,
    MachineView, OwnerSet, OwnerSource, Queued, Refusal, Reply, TopReply, WaitCondition,
    WaitOutcome, WaitReply, WaitRequest, WaitingOn, envelope, reply,
};
use crew_sys::display::{self, Desktop};
use crew_sys::idle::IdleEvent;
use crew_sys::machine;
use crew_sys::scope::{self, KillSignal};

use crate::admission::{self, Facts, Running, Verdict, Waiting};
use crate::agents::{self, Agent, Collector, Planner};
use crate::config::Config;
use crate::log::Log;
use crate::sizes::{self, Sizes};
use crate::waits::{self, Fate, JobState};

const TICK: Duration = Duration::from_secs(2);
const RENOTIFY_MS: u64 = 30_000;
const MIN_LEARNED_CORES_MILLI: u32 = 100;
const FATE_HISTORY: usize = 1024;

#[derive(Clone)]
pub struct Conn(Arc<Mutex<UnixStream>>);

impl Conn {
    pub fn new(stream: UnixStream) -> io::Result<Self> {
        stream.set_write_timeout(Some(Duration::from_secs(2)))?;
        Ok(Self(Arc::new(Mutex::new(stream))))
    }

    pub fn same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    fn send(&self, body: envelope::Body) {
        let mut s = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _ = crew_proto::write_frame(&mut *s, &Envelope::of(body));
    }
}

pub enum Msg {
    Job { req: JobRequest, conn: Conn },
    Done { done: JobDone },
    Gone { job_id: String },
    Top { conn: Conn },
    Owner { set: OwnerSet, conn: Conn },
    Wait { req: WaitRequest, conn: Conn },
    Hangup { conn: Conn },
    Idle(IdleEvent),
    Stop,
}

struct QueuedJob {
    req: JobRequest,
    kind: JobKind,
    conn: Conn,
    queued_at_ms: u64,
    expected_mem: u64,
    expected_cores: f64,
    waiting: WaitingOn,
    last_notice_ms: Option<u64>,
}

struct RunningJob {
    req: JobRequest,
    kind: JobKind,
    conn: Conn,
    scope_name: String,
    started_ms: u64,
    expected_mem: u64,
    expected_cores: f64,
    base_limits: Limits,
    stats: scope::Stats,
}

struct PendingWait {
    conn: Conn,
    condition: WaitCondition,
    job_id: String,
    since_ms: u64,
    deadline_ms: Option<u64>,
}

#[derive(Default)]
struct Owner {
    manual: Option<bool>,
    idle: Option<bool>,
}

impl Owner {
    fn present(&self) -> bool {
        self.manual.or(self.idle).unwrap_or(true)
    }

    fn source(&self) -> OwnerSource {
        match (self.manual, self.idle) {
            (Some(_), _) => OwnerSource::Manual,
            (None, Some(_)) => OwnerSource::WaylandIdle,
            (None, None) => OwnerSource::Default,
        }
    }
}

#[derive(Default)]
struct CpuLoad {
    last: Option<(u64, u64, u64)>,
    unmanaged_cores: f64,
    psi_cpu: f64,
}

/// Agent sessions seen by the scanner thread; the thread ends when the sender is dropped.
struct AgentWatch {
    latest: Arc<Mutex<Vec<Agent>>>,
    _stop: Sender<()>,
}

impl AgentWatch {
    fn start(mut collector: Collector) -> Self {
        let latest = Arc::new(Mutex::new(Vec::new()));
        let (stop, quit) = mpsc::channel::<()>();
        let shared = Arc::clone(&latest);
        thread::Builder::new()
            .name("agents".into())
            .spawn(move || {
                loop {
                    let seen = collector.collect(Instant::now());
                    *shared
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = seen;
                    if !matches!(quit.recv_timeout(TICK), Err(RecvTimeoutError::Timeout)) {
                        return;
                    }
                }
            })
            .expect("spawn agent scanner");
        Self {
            latest,
            _stop: stop,
        }
    }

    fn agents(&self) -> Vec<Agent> {
        self.latest
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

pub struct Daemon {
    cfg: Config,
    sizes_path: PathBuf,
    sizes: Sizes,
    log: Arc<Log>,
    started: Instant,
    queue: Vec<QueuedJob>,
    running: Vec<RunningJob>,
    owner: Owner,
    load: CpuLoad,
    finished_cpu_usec: u64,
    waits: Vec<PendingWait>,
    fates: VecDeque<(String, Fate)>,
    agents: Option<AgentWatch>,
    planner: Planner,
}

fn clean_id(id: &str) -> String {
    id.chars()
        .filter(char::is_ascii_alphanumeric)
        .take(12)
        .collect()
}

fn command_of(req: &JobRequest) -> String {
    req.argv.join(" ")
}

fn nonzero(v: u32, fallback: u32) -> u32 {
    if v == 0 { fallback } else { v }
}

impl Daemon {
    pub fn new(cfg: Config, sizes_path: PathBuf, log: Arc<Log>) -> Self {
        let sizes = Sizes::load(&sizes_path);
        if cfg.adopt.move_ || cfg.adopt.freeze {
            log.line(
                "WARN adopt.move and adopt.freeze are not implemented yet: dry-run log lines only",
            );
        }
        let agents = cfg.adopt.enabled.then(|| {
            AgentWatch::start(Collector::new(
                std::path::Path::new("/proc"),
                std::path::Path::new("/sys/fs/cgroup"),
                cfg.adopt.protect.clone(),
            ))
        });
        Self {
            cfg,
            sizes_path,
            sizes,
            log,
            started: Instant::now(),
            queue: Vec::new(),
            running: Vec::new(),
            owner: Owner::default(),
            load: CpuLoad::default(),
            finished_cpu_usec: 0,
            waits: Vec::new(),
            fates: VecDeque::new(),
            agents,
            planner: Planner::default(),
        }
    }

    fn now_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    pub fn run(mut self, rx: &Receiver<Msg>) {
        loop {
            match rx.recv_timeout(self.next_wake()) {
                Ok(Msg::Stop) | Err(RecvTimeoutError::Disconnected) => return,
                Ok(msg) => {
                    self.handle(msg);
                    self.reschedule();
                    self.settle_waits();
                }
                Err(RecvTimeoutError::Timeout) => self.tick(),
            }
        }
    }

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Job { req, conn } => self.enqueue(req, conn),
            Msg::Done { done } => self.finish(&done),
            Msg::Gone { job_id } => self.lose(&job_id),
            Msg::Top { conn } => {
                let reply = self.top();
                conn.send(envelope::Body::TopReply(reply));
            }
            Msg::Owner { set, conn } => {
                match OwnerSource::try_from(set.source) {
                    Ok(OwnerSource::Auto) => self.set_owner(|o| o.manual = None),
                    _ => self.set_owner(|o| o.manual = Some(set.present)),
                }
                let msg = format!(
                    "owner {} ({})",
                    if self.owner.present() {
                        "present"
                    } else {
                        "away"
                    },
                    source_name(self.owner.source())
                );
                conn.send(envelope::Body::Ack(Ack {
                    ok: true,
                    message: msg,
                }));
            }
            Msg::Wait { req, conn } => self.begin_wait(&req, conn),
            Msg::Hangup { conn } => self.waits.retain(|w| !w.conn.same(&conn)),
            Msg::Idle(ev) => self.set_owner(|o| o.idle = Some(ev == IdleEvent::Resumed)),
            Msg::Stop => {}
        }
    }

    fn set_owner(&mut self, change: impl FnOnce(&mut Owner)) {
        let before = self.owner.present();
        change(&mut self.owner);
        let now = self.owner.present();
        self.log.line(&format!(
            "OWNER present={now} source={}",
            source_name(self.owner.source())
        ));
        if before != now {
            self.apply_weights(now);
        }
    }

    fn apply_weights(&self, owner_present: bool) {
        for r in &self.running {
            let (cpu, io) = self.weights_for(&r.base_limits, owner_present);
            scope::set_weights(&r.scope_name, cpu, io);
        }
    }

    fn weights_for(&self, base: &Limits, owner_present: bool) -> (u32, u32) {
        if owner_present {
            (
                base.cpu_weight.min(self.cfg.owner_present_weights.cpu),
                base.io_weight.min(self.cfg.owner_present_weights.io),
            )
        } else {
            (base.cpu_weight, base.io_weight)
        }
    }

    fn expectations(&self, req: &JobRequest, kind: JobKind) -> (u64, f64) {
        let spec = self.cfg.kind(kind);
        let learned = self.sizes.expected(&sizes::key(req));
        let declared = req.declared.unwrap_or_default();
        let mem = learned
            .map(|l| l.peak_mem_bytes)
            .filter(|m| *m > 0)
            .or(declared.mem_bytes)
            .unwrap_or(spec.expected_mem);
        let cores = learned
            .map(|l| f64::from(l.cores_milli.max(MIN_LEARNED_CORES_MILLI)) / 1000.0)
            .or(declared.cpus.map(f64::from))
            .unwrap_or(spec.expected_cores);
        (mem, cores)
    }

    fn enqueue(&mut self, req: JobRequest, conn: Conn) {
        let kind = JobKind::try_from(req.kind).unwrap_or(JobKind::Unspecified);
        let (expected_mem, expected_cores) = self.expectations(&req, kind);
        self.queue.push(QueuedJob {
            req,
            kind,
            conn,
            queued_at_ms: self.now_ms(),
            expected_mem,
            expected_cores,
            waiting: WaitingOn::Unspecified,
            last_notice_ms: None,
        });
    }

    fn facts(&self) -> Facts {
        let snap = machine::snapshot(&[]).unwrap_or_default();
        let free_disk = self
            .cfg
            .watched_paths
            .iter()
            .filter_map(|p| machine::disk_free(p).ok().map(|f| (p.clone(), f)))
            .collect();
        Facts {
            mem_available: snap.mem_available,
            cpu_count: snap.cpu_count.max(1),
            unmanaged_cores: self.load.unmanaged_cores,
            free_disk,
            owner_present: self.owner.present(),
            now_ms: self.now_ms(),
        }
    }

    fn reschedule(&mut self) {
        if self.queue.is_empty() {
            return;
        }
        let facts = self.facts();
        let running: Vec<Running> = self
            .running
            .iter()
            .map(|r| Running {
                kind: r.kind,
                expected_mem: r.expected_mem,
                expected_cores: r.expected_cores,
                observed_peak: r.stats.mem_peak,
            })
            .collect();
        let waiting: Vec<Waiting> = self
            .queue
            .iter()
            .map(|q| Waiting {
                id: q.req.id.clone(),
                kind: q.kind,
                expected_mem: q.expected_mem,
                expected_cores: q.expected_cores,
                needs_display: q.req.needs_display,
                queued_at_ms: q.queued_at_ms,
            })
            .collect();
        let verdicts = admission::schedule(&self.cfg, &facts, &running, &waiting);
        let mut position = 0u32;
        for (id, verdict) in verdicts {
            let Some(at) = self.queue.iter().position(|q| q.req.id == id) else {
                continue;
            };
            match verdict {
                Verdict::Grant => {
                    let job = self.queue.remove(at);
                    self.grant(job, facts.owner_present);
                }
                Verdict::Refuse(reason, detail) => {
                    let job = self.queue.remove(at);
                    self.remember(&job.req.id, Fate::Refused);
                    self.log.line(&format!(
                        "REFUSE id={} kind={} reason={} detail={detail:?}",
                        job.req.id,
                        job.kind.name(),
                        reason.name()
                    ));
                    job.conn.send(envelope::Body::Reply(Reply {
                        decision: Decision::Refused as i32,
                        outcome: Some(reply::Outcome::Refusal(Refusal {
                            reason: reason as i32,
                            detail,
                        })),
                    }));
                }
                Verdict::Wait(reason, detail) => {
                    position += 1;
                    self.notify_waiting(at, reason, &detail, position, facts.now_ms);
                }
            }
        }
    }

    fn notify_waiting(
        &mut self,
        at: usize,
        reason: WaitingOn,
        detail: &str,
        position: u32,
        now_ms: u64,
    ) {
        let job = &mut self.queue[at];
        let changed = job.waiting != reason;
        let due = job
            .last_notice_ms
            .is_none_or(|t| now_ms.saturating_sub(t) >= RENOTIFY_MS);
        job.waiting = reason;
        if !(changed || due) {
            return;
        }
        if job.last_notice_ms.is_none() || changed {
            self.log.line(&format!(
                "QUEUE id={} kind={} project={} waiting={} detail={detail:?}",
                job.req.id,
                job.kind.name(),
                job.req.project,
                reason.name()
            ));
        }
        job.last_notice_ms = Some(now_ms);
        job.conn.send(envelope::Body::Reply(Reply {
            decision: Decision::Queued as i32,
            outcome: Some(reply::Outcome::Queued(Queued {
                waiting_on: reason as i32,
                position,
                queued_ms: now_ms.saturating_sub(job.queued_at_ms),
                detail: detail.to_string(),
            })),
        }));
    }

    fn grant(&mut self, job: QueuedJob, owner_present: bool) {
        self.remember(&job.req.id, Fate::Granted);
        let spec = self.cfg.kind(job.kind);
        let declared = job.req.declared.unwrap_or_default();
        let base_limits = Limits {
            mem_bytes: declared.mem_bytes.or(spec.mem_cap),
            cpu_weight: nonzero(declared.cpu_weight, spec.cpu_weight),
            io_weight: nonzero(declared.io_weight, spec.io_weight),
            cpus: declared.cpus,
        };
        let (cpu_weight, io_weight) = self.weights_for(&base_limits, owner_present);
        let limits = Limits {
            cpu_weight,
            io_weight,
            ..base_limits
        };

        let desktop = if job.req.hz_desktop && job.kind != JobKind::Bench {
            Desktop::Allowed
        } else {
            Desktop::Denied
        };
        let mut display_env: BTreeMap<String, String> =
            job.req.display_env.clone().into_iter().collect();
        let env_unset = display::sanitize(&mut display_env, desktop);
        let id = clean_id(&job.req.id);
        let scope_name = format!("crew-{}-{id}", job.kind.name());
        let tmpdir = format!("{}/{id}", self.cfg.tmp_root.trim_end_matches('/'));
        let grant = Grant {
            job_id: job.req.id.clone(),
            limits: Some(limits),
            scope_name: scope_name.clone(),
            env_set: [
                ("TMPDIR".to_string(), tmpdir),
                ("CREW_JOB".to_string(), job.req.id.clone()),
            ]
            .into(),
            env_unset,
        };
        self.log.line(&format!(
            "GRANT id={} kind={} project={} agent={} scope={scope_name} mem={}MiB cores={:.1} owner_present={owner_present} cmd={:?}",
            job.req.id,
            job.kind.name(),
            job.req.project,
            job.req.agent,
            job.expected_mem >> 20,
            job.expected_cores,
            command_of(&job.req),
        ));
        job.conn.send(envelope::Body::Reply(Reply {
            decision: Decision::Granted as i32,
            outcome: Some(reply::Outcome::Grant(grant)),
        }));
        self.running.push(RunningJob {
            req: job.req,
            kind: job.kind,
            conn: job.conn,
            scope_name,
            started_ms: self.started.elapsed().as_millis() as u64,
            expected_mem: job.expected_mem,
            expected_cores: job.expected_cores,
            base_limits,
            stats: scope::Stats::default(),
        });
    }

    fn finish(&mut self, done: &JobDone) {
        let Some(at) = self.running.iter().position(|r| r.req.id == done.job_id) else {
            return;
        };
        let job = self.running.remove(at);
        self.finished_cpu_usec += done.cpu_usec;
        if done.peak_mem_bytes > 0 && done.wall_ms > 0 {
            let cores_milli = u32::try_from(done.cpu_usec / done.wall_ms).unwrap_or(u32::MAX);
            self.sizes
                .record(&sizes::key(&job.req), done.peak_mem_bytes, cores_milli);
            if let Err(e) = self.sizes.save(&self.sizes_path) {
                self.log.line(&format!("WARN could not save sizes: {e}"));
            }
        }
        self.log.line(&format!(
            "DONE id={} kind={} exit={} wall_ms={} cpu_ms={} peak_mib={} io_read_mib={} io_write_mib={}",
            done.job_id,
            job.kind.name(),
            done.exit_code,
            done.wall_ms,
            done.cpu_usec / 1000,
            done.peak_mem_bytes >> 20,
            done.io_read_bytes >> 20,
            done.io_write_bytes >> 20,
        ));
        job.conn.send(envelope::Body::Ack(Ack {
            ok: true,
            message: String::new(),
        }));
    }

    fn lose(&mut self, job_id: &str) {
        if let Some(at) = self.queue.iter().position(|q| q.req.id == job_id) {
            self.queue.remove(at);
            self.remember(job_id, Fate::Gone);
            self.log.line(&format!("GONE id={job_id} while queued"));
        } else if let Some(at) = self.running.iter().position(|r| r.req.id == job_id) {
            let job = self.running.remove(at);
            scope::kill(&job.scope_name, KillSignal::Kill);
            self.log
                .line(&format!("GONE id={job_id} while running; scope killed"));
        }
    }

    fn tick(&mut self) {
        for r in &mut self.running {
            if let Some(s) = scope::stats(&r.scope_name) {
                r.stats = s;
            }
        }
        self.update_load();
        self.reschedule();
        self.adopt_dry_run();
        self.settle_waits();
    }

    fn next_wake(&self) -> Duration {
        let now = self.now_ms();
        self.waits
            .iter()
            .filter_map(|w| w.deadline_ms)
            .map(|d| Duration::from_millis(d.saturating_sub(now)))
            .fold(TICK, Duration::min)
            .max(Duration::from_millis(1))
    }

    fn remember(&mut self, id: &str, fate: Fate) {
        if self.fates.len() == FATE_HISTORY {
            self.fates.pop_front();
        }
        self.fates.push_back((id.to_string(), fate));
    }

    fn job_state(&self, id: &str) -> JobState {
        if self.queue.iter().any(|q| q.req.id == id) {
            return JobState::Queued;
        }
        match self.fates.iter().rev().find(|(f, _)| f == id) {
            Some((_, Fate::Granted)) => JobState::Granted,
            Some((_, Fate::Refused)) => JobState::Refused,
            Some((_, Fate::Gone)) => JobState::Gone,
            None => JobState::Unknown,
        }
    }

    fn begin_wait(&mut self, req: &WaitRequest, conn: Conn) {
        let condition =
            WaitCondition::try_from(req.condition).unwrap_or(WaitCondition::Unspecified);
        let now = self.now_ms();
        self.waits.push(PendingWait {
            conn,
            condition,
            job_id: req.job_id.clone(),
            since_ms: now,
            deadline_ms: (req.timeout_ms > 0).then(|| now + req.timeout_ms),
        });
    }

    /// Answers every wait whose condition now holds, is malformed, or has run out of time.
    fn settle_waits(&mut self) {
        if self.waits.is_empty() {
            return;
        }
        let now = self.now_ms();
        let job = |id: &str| self.job_state(id);
        let facts = waits::Facts {
            queue_empty: self.queue.is_empty(),
            owner_present: self.owner.present(),
            job: &job,
        };
        let mut answers = Vec::new();
        for (at, w) in self.waits.iter().enumerate() {
            let verdict = match waits::evaluate(w.condition, &w.job_id, &facts) {
                Err(detail) => Some((WaitOutcome::Rejected, detail)),
                Ok(Some(v)) => Some(v),
                Ok(None) if w.deadline_ms.is_some_and(|d| now >= d) => {
                    Some((WaitOutcome::TimedOut, "timed out".to_string()))
                }
                Ok(None) => None,
            };
            if let Some(v) = verdict {
                answers.push((at, v));
            }
        }
        for (at, (outcome, detail)) in answers.into_iter().rev() {
            let w = self.waits.remove(at);
            w.conn.send(envelope::Body::WaitReply(WaitReply {
                outcome: outcome as i32,
                condition: w.condition as i32,
                detail,
                waited_ms: now.saturating_sub(w.since_ms),
            }));
        }
    }

    fn adopt_dry_run(&mut self) {
        let Some(watch) = &self.agents else { return };
        let seen = watch.agents();
        let notes = self.planner.plan(
            &self.cfg.adopt,
            &seen,
            self.owner.present(),
            self.load.psi_cpu,
        );
        for note in notes {
            self.log.line(&note.line());
        }
    }

    fn update_load(&mut self) {
        let Ok(snap) = machine::snapshot(&[]) else {
            return;
        };
        self.load.psi_cpu = snap.psi.cpu_some_avg10;
        let now_ms = self.now_ms();
        let scopes =
            self.finished_cpu_usec + self.running.iter().map(|r| r.stats.cpu_usec).sum::<u64>();
        if let Some((at, busy, scoped)) = self.load.last {
            let dt_usec = now_ms.saturating_sub(at) * 1000;
            if dt_usec > 0 {
                let busy_d = snap.cpu_busy_usec.saturating_sub(busy);
                let scope_d = scopes.saturating_sub(scoped);
                self.load.unmanaged_cores = busy_d.saturating_sub(scope_d) as f64 / dt_usec as f64;
            }
        }
        self.load.last = Some((now_ms, snap.cpu_busy_usec, scopes));
    }

    fn top(&self) -> TopReply {
        let now = self.now_ms();
        let snap = machine::snapshot(&[]).unwrap_or_default();
        let view = |req: &JobRequest,
                    kind: JobKind,
                    mem: u64,
                    cores: f64,
                    age_ms: u64,
                    waiting: WaitingOn,
                    s: scope::Stats| JobView {
            id: req.id.clone(),
            kind: kind as i32,
            agent: req.agent.clone(),
            project: req.project.clone(),
            command: command_of(req),
            expected_mem_bytes: mem,
            expected_cores_milli: (cores * 1000.0) as u32,
            age_ms,
            cpu_usec: s.cpu_usec,
            mem_bytes: s.mem_current,
            io_bytes: s.io_read + s.io_write,
            waiting_on: waiting as i32,
        };
        let running = self
            .running
            .iter()
            .map(|r| {
                view(
                    &r.req,
                    r.kind,
                    r.expected_mem,
                    r.expected_cores,
                    now.saturating_sub(r.started_ms),
                    WaitingOn::Unspecified,
                    r.stats,
                )
            })
            .collect();
        let queued = self
            .queue
            .iter()
            .map(|q| {
                view(
                    &q.req,
                    q.kind,
                    q.expected_mem,
                    q.expected_cores,
                    now.saturating_sub(q.queued_at_ms),
                    q.waiting,
                    scope::Stats::default(),
                )
            })
            .collect();
        let disk = self
            .cfg
            .watched_paths
            .iter()
            .filter_map(|p| {
                let free = machine::disk_free(p).ok()?;
                Some(DiskView {
                    path: p.clone(),
                    free_bytes: free,
                    min_free_bytes: self.cfg.min_free_disk(p).unwrap_or(0),
                })
            })
            .collect();
        let seen = self
            .agents
            .as_ref()
            .map(AgentWatch::agents)
            .unwrap_or_default();
        let mut jobs: BTreeMap<String, (u32, u32)> = BTreeMap::new();
        for r in &self.running {
            jobs.entry(r.req.project.clone()).or_default().0 += 1;
        }
        for q in &self.queue {
            jobs.entry(q.req.project.clone()).or_default().1 += 1;
        }
        TopReply {
            agents: seen.iter().map(Agent::view).collect(),
            projects: agents::project_views(&seen, &jobs),
            running,
            queued,
            machine: Some(MachineView {
                mem_total: snap.mem_total,
                mem_available: snap.mem_available,
                cpu_count: snap.cpu_count,
                load1: snap.load1,
                psi_cpu_some_avg10: snap.psi.cpu_some_avg10,
                psi_mem_some_avg10: snap.psi.mem_some_avg10,
                psi_io_some_avg10: snap.psi.io_some_avg10,
                disk,
                owner_present: self.owner.present(),
                owner_source: self.owner.source() as i32,
                unmanaged_cpu_pct: self.load.unmanaged_cores / f64::from(snap.cpu_count.max(1))
                    * 100.0,
            }),
        }
    }
}

fn source_name(s: OwnerSource) -> &'static str {
    match s {
        OwnerSource::Manual => "manual",
        OwnerSource::WaylandIdle => "wayland idle",
        OwnerSource::Default => "default",
        OwnerSource::Auto => "auto",
        OwnerSource::Unspecified => "unknown",
    }
}
