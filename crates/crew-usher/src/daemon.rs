use std::collections::{BTreeMap, VecDeque};
use std::fs::{self, OpenOptions};
use std::io;
use std::os::unix::net::UnixStream;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crew_proto::{
    Ack, Decision, DetachedAccepted, DetachedJob, DiskView, Envelope, Grant, JobDone, JobKind,
    JobPhase, JobRequest, JobSummary, JobView, JobsReply, Limits, MachineView, OwnerSet,
    OwnerSource, Queued, Refusal, RefusalReason, Reply, TopReply, WaitCondition, WaitOutcome,
    WaitReply, WaitRequest, WaitingOn, envelope, reply,
};
use crew_sys::display::{self, Desktop};
use crew_sys::idle::IdleEvent;
use crew_sys::machine;
use crew_sys::scope::{self, KillSignal, ScopeLimits};

use crate::admission::{self, Facts, Running, Verdict, Waiting};
use crate::agents::{self, Agent, Collector, Planner};
use crate::config::Config;
use crate::detached::{self, Dirs};
use crate::log::Log;
use crate::pool::{self, PoolHold};
use crate::sizes::{self, Sizes};
use crate::waits::{self, Fate, JobState};

const TICK: Duration = Duration::from_secs(2);
const RENOTIFY_MS: u64 = 30_000;
const MIN_LEARNED_CORES_MILLI: u32 = 100;
const FATE_HISTORY: usize = 1024;
/// A client that lost the usher may claim its old queue time from a daemon that started this recently.
const RESUBMIT_GRACE: Duration = Duration::from_secs(10 * 60);
/// A job started this recently may not have its scope yet, so a missing scope proves nothing.
const SCOPE_START_GRACE_MS: u64 = 10_000;
/// A cancelled job gets SIGTERM, then SIGKILL after this long.
const CANCEL_GRACE_MS: u64 = 10_000;

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
    Job {
        req: Box<JobRequest>,
        conn: Conn,
    },
    Done {
        done: JobDone,
    },
    /// The connection that carried `job_id` closed.
    Gone {
        job_id: String,
        conn: Conn,
    },
    /// A detached job's process ended; `code` is `None` when it could not be waited for.
    Exited {
        job_id: String,
        code: Option<i32>,
    },
    Jobs {
        conn: Conn,
    },
    Cancel {
        job_id: String,
        conn: Conn,
    },
    Top {
        conn: Conn,
    },
    Owner {
        set: OwnerSet,
        conn: Conn,
    },
    Wait {
        req: WaitRequest,
        conn: Conn,
    },
    Hangup {
        conn: Conn,
    },
    Idle(IdleEvent),
    Stop,
}

/// Who owns a job: a `crew run` client on a connection, or the usher itself.
#[derive(Clone)]
enum Origin {
    Attached(Conn),
    Detached(Files),
}

#[derive(Clone)]
struct Files {
    log: PathBuf,
    status: PathBuf,
}

/// How the usher learns that a running detached job ended.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Watch {
    /// A thread of this usher waits for the process.
    Child,
    /// The job outlived an earlier usher; its scope is polled and the wrapper's status file read.
    Scope,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Cancel {
    No,
    Terminated { at_ms: u64 },
    Killed,
}

struct QueuedJob {
    req: JobRequest,
    kind: JobKind,
    origin: Origin,
    /// Unix ms of the first time this job was queued, kept across usher restarts.
    queued_at_ms: u64,
    expected_mem: u64,
    expected_cores: f64,
    waiting: WaitingOn,
    last_notice_ms: Option<u64>,
    position: u32,
    detail: String,
}

struct RunningJob {
    req: JobRequest,
    kind: JobKind,
    origin: Origin,
    scope_name: String,
    queued_at_ms: u64,
    started_ms: u64,
    expected_mem: u64,
    expected_cores: f64,
    base_limits: Limits,
    stats: scope::Stats,
    watch: Watch,
    cancel: Cancel,
    tmpdir: Option<PathBuf>,
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

/// Where the daemon keeps what must outlive it.
#[derive(Clone, Debug)]
pub struct StatePaths {
    pub sizes: PathBuf,
    pub jobs: PathBuf,
    pub job_logs: PathBuf,
}

pub struct Daemon {
    cfg: Config,
    sizes_path: PathBuf,
    jobs_path: PathBuf,
    dirs: Dirs,
    events: Sender<Msg>,
    sizes: Sizes,
    log: Arc<Log>,
    started: Instant,
    /// Unix ms when `started` was taken; the daemon clock is this plus the time since.
    boot_unix_ms: u64,
    queue: Vec<QueuedJob>,
    finished: Vec<DetachedJob>,
    running: Vec<RunningJob>,
    owner: Owner,
    load: CpuLoad,
    finished_cpu_usec: u64,
    waits: Vec<PendingWait>,
    fates: VecDeque<(String, Fate)>,
    agents: Option<AgentWatch>,
    planner: Planner,
    pool: PoolHold,
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

/// How other jobs' messages name this one: `bench 1a2b3c4d (horizon: ./m.sh)`.
fn label_of(req: &JobRequest, kind: JobKind) -> String {
    let id: String = req.id.chars().take(8).collect();
    let mut cmd: String = command_of(req).chars().take(40).collect();
    if command_of(req).chars().count() > 40 {
        cmd.push('…');
    }
    format!("{} {id} ({}: {cmd})", kind.name(), req.project)
}

fn exit_code_of(status: ExitStatus) -> i32 {
    status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn nonzero(v: u32, fallback: u32) -> u32 {
    if v == 0 { fallback } else { v }
}

impl Daemon {
    pub fn new(cfg: Config, paths: StatePaths, log: Arc<Log>, events: Sender<Msg>) -> Self {
        let sizes = Sizes::load(&paths.sizes);
        let sizes_path = paths.sizes;
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
        let mut daemon = Self {
            cfg,
            sizes_path,
            jobs_path: paths.jobs.clone(),
            dirs: Dirs {
                state: paths
                    .jobs
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_default(),
                logs: paths.job_logs,
            },
            events,
            sizes,
            log,
            started: Instant::now(),
            boot_unix_ms: unix_ms(),
            queue: Vec::new(),
            finished: Vec::new(),
            running: Vec::new(),
            owner: Owner::default(),
            load: CpuLoad::default(),
            finished_cpu_usec: 0,
            waits: Vec::new(),
            fates: VecDeque::new(),
            agents,
            planner: Planner::default(),
            pool: PoolHold::default(),
        };
        daemon.restore();
        daemon
    }

    fn sync_pool(&mut self) {
        let mbx = self.cfg.depot.find_mbx();
        for line in self.pool.sync(&self.cfg.depot, self.owner.present(), mbx) {
            self.log.line(&line);
        }
    }

    /// Unix ms, steady across wall-clock steps; queue times are on this clock so they survive a restart.
    fn now_ms(&self) -> u64 {
        self.boot_unix_ms + self.started.elapsed().as_millis() as u64
    }

    pub fn run(mut self, rx: &Receiver<Msg>) {
        self.sync_pool();
        let mut next_tick = Instant::now() + TICK;
        loop {
            match rx.recv_timeout(self.next_wake(next_tick)) {
                Ok(Msg::Stop) | Err(RecvTimeoutError::Disconnected) => return,
                Ok(msg) => {
                    self.handle(msg);
                    self.reschedule();
                    self.settle_waits();
                }
                Err(RecvTimeoutError::Timeout) => self.settle_waits(),
            }
            // The tick has its own deadline: a steady stream of messages must not postpone it.
            if Instant::now() >= next_tick {
                self.tick();
                next_tick = Instant::now() + TICK;
            }
        }
    }

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Job { req, conn } => self.enqueue(*req, conn),
            Msg::Done { done } => self.finish(&done),
            Msg::Gone { job_id, conn } => self.lose(&job_id, &conn),
            Msg::Exited { job_id, code } => self.end_detached(&job_id, code),
            Msg::Jobs { conn } => {
                let reply = self.jobs();
                conn.send(envelope::Body::JobsReply(reply));
            }
            Msg::Cancel { job_id, conn } => {
                let ack = self.cancel(&job_id);
                conn.send(envelope::Body::Ack(ack));
            }
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
            self.sync_pool();
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

    fn insert_queued(&mut self, job: QueuedJob) {
        let at = self
            .queue
            .partition_point(|q| q.queued_at_ms <= job.queued_at_ms);
        self.queue.insert(at, job);
    }

    fn queued_job(&self, req: JobRequest, origin: Origin, queued_at_ms: u64) -> QueuedJob {
        let kind = JobKind::try_from(req.kind).unwrap_or(JobKind::Unspecified);
        let (expected_mem, expected_cores) = self.expectations(&req, kind);
        QueuedJob {
            req,
            kind,
            origin,
            queued_at_ms,
            expected_mem,
            expected_cores,
            waiting: WaitingOn::Unspecified,
            last_notice_ms: None,
            position: 0,
            detail: String::new(),
        }
    }

    fn enqueue(&mut self, req: JobRequest, conn: Conn) {
        if req.detach {
            return self.enqueue_detached(req, &conn);
        }
        if self.running.iter().any(|r| r.req.id == req.id) {
            conn.send(envelope::Body::Ack(Ack {
                ok: false,
                message: format!("job {} is already running", req.id),
            }));
            return;
        }
        // The same client coming back after losing its connection keeps its place; the stale entry just gets the new connection.
        if let Some(q) = self.queue.iter_mut().find(|q| q.req.id == req.id) {
            q.origin = Origin::Attached(conn);
            q.last_notice_ms = None;
            return;
        }
        let now = self.now_ms();
        let claimed = req.queued_at_unix_ms;
        let at = if claimed > 0 && self.started.elapsed() < RESUBMIT_GRACE {
            claimed.min(now)
        } else {
            now
        };
        if at < now {
            self.log.line(&format!(
                "RESUBMIT id={} queued {}s ago",
                req.id,
                (now - at) / 1000
            ));
        }
        let job = self.queued_job(req, Origin::Attached(conn), at);
        self.insert_queued(job);
    }

    fn reject(conn: &Conn, message: String) {
        conn.send(envelope::Body::Ack(Ack { ok: false, message }));
    }

    fn enqueue_detached(&mut self, mut req: JobRequest, conn: &Conn) {
        let id_ok = !req.id.is_empty()
            && req
                .id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-');
        if !id_ok || req.argv.is_empty() {
            return Self::reject(conn, "a detached job needs an id and a command".into());
        }
        let log = if req.log_path.is_empty() {
            self.dirs.default_log(&req.id)
        } else {
            PathBuf::from(&req.log_path)
        };
        let files = Files {
            log: log.clone(),
            status: self.dirs.status(&req.id),
        };
        if self.job_state(&req.id) != JobState::Unknown {
            // A retried submit finds its job already here.
            conn.send(envelope::Body::DetachedAccepted(DetachedAccepted {
                job_id: req.id.clone(),
                log_path: log.display().to_string(),
            }));
            return;
        }
        if let Err(e) = fs::create_dir_all(&self.dirs.logs).and_then(|()| open_log(&log)) {
            return Self::reject(conn, format!("cannot write the log {}: {e}", log.display()));
        }
        req.log_path = log.display().to_string();
        let now = self.now_ms();
        let job = self.queued_job(req.clone(), Origin::Detached(files), now);
        self.log.line(&format!(
            "DETACH id={} kind={} project={} agent={} log={} cmd={:?}",
            req.id,
            job.kind.name(),
            req.project,
            req.agent,
            log.display(),
            command_of(&req),
        ));
        self.insert_queued(job);
        self.persist();
        conn.send(envelope::Body::DetachedAccepted(DetachedAccepted {
            job_id: req.id,
            log_path: log.display().to_string(),
        }));
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
                label: label_of(&r.req, r.kind),
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
                label: label_of(&q.req, q.kind),
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
                    match &job.origin {
                        Origin::Attached(conn) => conn.send(envelope::Body::Reply(Reply {
                            decision: Decision::Refused as i32,
                            outcome: Some(reply::Outcome::Refusal(Refusal {
                                reason: reason as i32,
                                detail,
                            })),
                        })),
                        Origin::Detached(files) => self.conclude_unstarted(
                            &job,
                            files,
                            JobPhase::Failed,
                            format!("refused ({}): {detail}", reason.name()),
                        ),
                    }
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
        job.position = position;
        job.detail = detail.to_string();
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
        if let Origin::Attached(conn) = &job.origin {
            conn.send(envelope::Body::Reply(Reply {
                decision: Decision::Queued as i32,
                outcome: Some(reply::Outcome::Queued(Queued {
                    waiting_on: reason as i32,
                    position,
                    queued_ms: now_ms.saturating_sub(job.queued_at_ms),
                    detail: detail.to_string(),
                })),
            }));
        }
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

        let desktop = if job.req.desktop && job.kind != JobKind::Bench {
            Desktop::Allowed
        } else {
            Desktop::Denied
        };
        let mut display_env: BTreeMap<String, String> =
            job.req.display_env.clone().into_iter().collect();
        let env_unset = display::sanitize(&mut display_env, desktop);
        let (build, reserve) = pool::terms(
            &self.cfg,
            &pool::Job {
                kind: job.kind,
                expected_cores: job.expected_cores,
                expected_mem: job.expected_mem,
                owner_present,
            },
            || self.cfg.depot.find_mbx(),
        );
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
            build,
            reserve,
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
        let origin = job.origin.clone();
        self.running.push(RunningJob {
            req: job.req,
            kind: job.kind,
            origin: job.origin,
            scope_name,
            queued_at_ms: job.queued_at_ms,
            started_ms: self.now_ms(),
            expected_mem: job.expected_mem,
            expected_cores: job.expected_cores,
            base_limits,
            stats: scope::Stats::default(),
            watch: Watch::Child,
            cancel: Cancel::No,
            tmpdir: None,
        });
        let at = self.running.len() - 1;
        match origin {
            Origin::Attached(conn) => conn.send(envelope::Body::Reply(Reply {
                decision: Decision::Granted as i32,
                outcome: Some(reply::Outcome::Grant(grant)),
            })),
            Origin::Detached(files) => match self.launch(&self.running[at], &grant, &files) {
                Ok(tmpdir) => {
                    self.running[at].tmpdir = tmpdir;
                    self.persist();
                }
                Err(e) => {
                    let job = self.running.remove(at);
                    self.log
                        .line(&format!("FAIL id={} could not start: {e}", job.req.id));
                    let rec = self.running_record(&job, &files);
                    self.conclude(rec, JobPhase::Failed, None, format!("could not start: {e}"));
                }
            },
        }
    }

    /// Starts a detached job in its scope with its output going to the log; returns the job's private temp dir.
    fn launch(
        &self,
        job: &RunningJob,
        grant: &Grant,
        files: &Files,
    ) -> io::Result<Option<PathBuf>> {
        let mut env: BTreeMap<String, String> = job.req.env.clone().into_iter().collect();
        for key in &grant.env_unset {
            env.remove(key);
        }
        let mut env_set = grant.env_set.clone();
        let tmpdir = env_set.get("TMPDIR").map(PathBuf::from);
        let tmpdir = tmpdir.filter(|dir| fs::create_dir_all(dir).is_ok());
        if tmpdir.is_none() {
            env_set.remove("TMPDIR");
        }
        env.extend(env_set);
        let _ = fs::remove_file(&files.status);
        let argv = scope::status_wrapped(&grant.reserved_argv(&job.req.argv), &files.status);
        let limits = grant.limits.unwrap_or_default();
        let child = scope::spawn_logged(
            &argv,
            Path::new(&job.req.cwd),
            &env,
            &ScopeLimits {
                mem_bytes: limits.mem_bytes,
                cpu_weight: limits.cpu_weight,
                io_weight: limits.io_weight,
                cpus: limits.cpus,
            },
            &grant.scope_name,
            open_log(&files.log)?,
        )?;
        let (tx, id) = (self.events.clone(), job.req.id.clone());
        thread::Builder::new()
            .name("job-wait".into())
            .spawn(move || {
                let mut child = child;
                let code = child.wait().ok().map(exit_code_of);
                let _ = tx.send(Msg::Exited { job_id: id, code });
            })?;
        Ok(tmpdir)
    }

    /// Books a finished job's usage and sizes, and logs it.
    fn note_done(&mut self, job: &RunningJob, done: &JobDone) {
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
    }

    fn finish(&mut self, done: &JobDone) {
        let Some(at) = self
            .running
            .iter()
            .position(|r| r.req.id == done.job_id && matches!(r.origin, Origin::Attached(_)))
        else {
            return;
        };
        let job = self.running.remove(at);
        self.note_done(&job, done);
        if let Origin::Attached(conn) = &job.origin {
            conn.send(envelope::Body::Ack(Ack {
                ok: true,
                message: String::new(),
            }));
        }
    }

    fn lose(&mut self, job_id: &str, from: &Conn) {
        let ours = |o: &Origin| matches!(o, Origin::Attached(c) if c.same(from));
        if let Some(at) = self
            .queue
            .iter()
            .position(|q| q.req.id == job_id && ours(&q.origin))
        {
            self.queue.remove(at);
            self.remember(job_id, Fate::Gone);
            self.log.line(&format!("GONE id={job_id} while queued"));
        } else if let Some(at) = self
            .running
            .iter()
            .position(|r| r.req.id == job_id && ours(&r.origin))
        {
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
        for line in self.pool.poll() {
            self.log.line(&line);
        }
        self.update_load();
        self.watch_detached();
        self.reschedule();
        self.adopt_dry_run();
        self.settle_waits();
    }

    fn next_wake(&self, next_tick: Instant) -> Duration {
        let now = self.now_ms();
        self.waits
            .iter()
            .filter_map(|w| w.deadline_ms)
            .map(|d| Duration::from_millis(d.saturating_sub(now)))
            .fold(
                next_tick.saturating_duration_since(Instant::now()),
                Duration::min,
            )
            .max(Duration::from_millis(1))
    }

    fn remember(&mut self, id: &str, fate: Fate) {
        if self.fates.len() == FATE_HISTORY {
            self.fates.pop_front();
        }
        self.fates.push_back((id.to_string(), fate));
    }

    fn job_state(&self, id: &str) -> JobState {
        if let Some(q) = self.queue.iter().find(|q| q.req.id == id) {
            return match q.origin {
                Origin::Attached(_) => JobState::Queued,
                Origin::Detached(_) => JobState::DetachedQueued,
            };
        }
        if self
            .running
            .iter()
            .any(|r| r.req.id == id && matches!(r.origin, Origin::Detached(_)))
        {
            return JobState::DetachedRunning;
        }
        if let Some(f) = self
            .finished
            .iter()
            .rev()
            .find(|f| f.request.as_ref().is_some_and(|r| r.id == id))
        {
            return match (detached::phase(f), f.exit_code) {
                (JobPhase::Exited, Some(code)) => JobState::DetachedExited(code),
                _ => JobState::DetachedEnded {
                    ran: f.started_at_unix_ms > 0,
                    reason: f.reason.clone(),
                },
            };
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
                Err(detail) => Some(waits::Verdict {
                    outcome: WaitOutcome::Rejected,
                    detail,
                    exit_code: None,
                }),
                Ok(Some(v)) => Some(v),
                Ok(None) if w.deadline_ms.is_some_and(|d| now >= d) => Some(waits::Verdict {
                    outcome: WaitOutcome::TimedOut,
                    detail: "timed out".to_string(),
                    exit_code: None,
                }),
                Ok(None) => None,
            };
            if let Some(v) = verdict {
                answers.push((at, v));
            }
        }
        for (at, v) in answers.into_iter().rev() {
            let w = self.waits.remove(at);
            w.conn.send(envelope::Body::WaitReply(WaitReply {
                outcome: v.outcome as i32,
                condition: w.condition as i32,
                detail: v.detail,
                waited_ms: now.saturating_sub(w.since_ms),
                exit_code: v.exit_code,
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

fn open_log(path: &Path) -> io::Result<fs::File> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    OpenOptions::new().create(true).append(true).open(path)
}

impl Daemon {
    fn base_record(
        &self,
        req: &JobRequest,
        files: &Files,
        queued_at_ms: u64,
        phase: JobPhase,
    ) -> DetachedJob {
        DetachedJob {
            request: Some(req.clone()),
            phase: phase as i32,
            queued_at_unix_ms: queued_at_ms,
            log_path: files.log.display().to_string(),
            status_path: files.status.display().to_string(),
            ..DetachedJob::default()
        }
    }

    fn queued_record(&self, q: &QueuedJob, files: &Files) -> DetachedJob {
        DetachedJob {
            expected_mem_bytes: q.expected_mem,
            expected_cores_milli: (q.expected_cores * 1000.0) as u32,
            ..self.base_record(&q.req, files, q.queued_at_ms, JobPhase::Queued)
        }
    }

    fn running_record(&self, r: &RunningJob, files: &Files) -> DetachedJob {
        DetachedJob {
            started_at_unix_ms: r.started_ms,
            scope_name: r.scope_name.clone(),
            base_limits: Some(r.base_limits),
            expected_mem_bytes: r.expected_mem,
            expected_cores_milli: (r.expected_cores * 1000.0) as u32,
            tmpdir: r
                .tmpdir
                .as_ref()
                .map(|d| d.display().to_string())
                .unwrap_or_default(),
            ..self.base_record(&r.req, files, r.queued_at_ms, JobPhase::Running)
        }
    }

    /// Writes the queued and running detached jobs and the recently finished ones to the state file.
    fn persist(&self) {
        let mut jobs = self.finished.clone();
        for r in &self.running {
            if let Origin::Detached(f) = &r.origin {
                jobs.push(self.running_record(r, f));
            }
        }
        for q in &self.queue {
            if let Origin::Detached(f) = &q.origin {
                jobs.push(self.queued_record(q, f));
            }
        }
        if let Err(e) = detached::save(&self.jobs_path, jobs) {
            self.log
                .line(&format!("WARN could not save the job state: {e}"));
        }
    }

    /// Moves a detached job's record to the finished list.
    fn conclude(
        &mut self,
        mut rec: DetachedJob,
        phase: JobPhase,
        exit_code: Option<i32>,
        reason: String,
    ) {
        rec.phase = phase as i32;
        rec.exit_code = exit_code;
        rec.reason = reason;
        rec.finished_at_unix_ms = self.now_ms();
        if !rec.tmpdir.is_empty() {
            let _ = fs::remove_dir_all(&rec.tmpdir);
        }
        if let Some(req) = rec.request.as_mut() {
            req.env.clear();
        }
        self.log.line(&format!(
            "END id={} phase={phase:?} exit={exit_code:?} reason={:?}",
            rec.request.as_ref().map_or("", |r| r.id.as_str()),
            rec.reason
        ));
        self.finished.push(rec);
        let now = self.now_ms();
        detached::prune(&mut self.finished, now, &self.dirs);
        self.persist();
    }

    /// A queued detached job that was removed from the queue without ever running.
    fn conclude_unstarted(
        &mut self,
        job: &QueuedJob,
        files: &Files,
        phase: JobPhase,
        reason: String,
    ) {
        let rec = self.queued_record(job, files);
        self.conclude(rec, phase, None, reason);
    }

    /// Takes back the jobs an earlier usher left: queued ones keep their time, running ones are watched again.
    fn restore(&mut self) {
        let records = detached::load(&self.jobs_path);
        if records.is_empty() {
            return;
        }
        let (mut queued, mut adopted, mut ended) = (0, 0, 0);
        for rec in records {
            let Some(req) = rec.request.clone() else {
                continue;
            };
            let files = Files {
                log: PathBuf::from(&rec.log_path),
                status: PathBuf::from(&rec.status_path),
            };
            match detached::phase(&rec) {
                JobPhase::Queued => {
                    let job = self.queued_job(req, Origin::Detached(files), rec.queued_at_unix_ms);
                    self.queue.push(job);
                    queued += 1;
                }
                JobPhase::Running
                    if scope::alive(&rec.scope_name)
                        || (self.now_ms().saturating_sub(rec.started_at_unix_ms)
                            < SCOPE_START_GRACE_MS
                            && scope::read_status(&files.status).is_none()) =>
                {
                    self.running.push(RunningJob {
                        req,
                        kind: JobKind::try_from(rec.request.as_ref().map_or(0, |r| r.kind))
                            .unwrap_or(JobKind::Unspecified),
                        origin: Origin::Detached(files),
                        scope_name: rec.scope_name.clone(),
                        queued_at_ms: rec.queued_at_unix_ms,
                        started_ms: rec.started_at_unix_ms,
                        expected_mem: rec.expected_mem_bytes,
                        expected_cores: f64::from(rec.expected_cores_milli) / 1000.0,
                        base_limits: rec.base_limits.unwrap_or_default(),
                        stats: scope::Stats::default(),
                        watch: Watch::Scope,
                        cancel: Cancel::No,
                        tmpdir: (!rec.tmpdir.is_empty()).then(|| PathBuf::from(&rec.tmpdir)),
                    });
                    adopted += 1;
                }
                JobPhase::Running => {
                    // The job ended while no usher was watching: its wrapper's status file still says how.
                    let code = scope::read_status(&files.status);
                    let (phase, reason) = match code {
                        Some(_) => (JobPhase::Exited, String::new()),
                        None => (
                            JobPhase::Failed,
                            "the usher restarted and the job's scope was gone with no exit status"
                                .to_string(),
                        ),
                    };
                    self.conclude(rec, phase, code, reason);
                    ended += 1;
                }
                JobPhase::Exited | JobPhase::Failed | JobPhase::Cancelled => {
                    self.finished.push(rec);
                }
                JobPhase::Unspecified => {}
            }
        }
        self.queue.sort_by_key(|q| q.queued_at_ms);
        self.finished.sort_by_key(|f| f.finished_at_unix_ms);
        self.log.line(&format!(
            "RESTORE queued={queued} adopted={adopted} ended_while_down={ended} finished={}",
            self.finished.len()
        ));
        self.persist();
    }

    /// A detached job's process ended: record how, free its slot, and learn its size.
    fn end_detached(&mut self, id: &str, code: Option<i32>) {
        let Some(at) = self
            .running
            .iter()
            .position(|r| r.req.id == id && matches!(r.origin, Origin::Detached(_)))
        else {
            return;
        };
        let job = self.running.remove(at);
        let Origin::Detached(files) = &job.origin else {
            return;
        };
        let reported = code.or_else(|| scope::read_status(&files.status));
        if let Some(exit_code) = reported {
            let done = JobDone {
                job_id: id.to_string(),
                exit_code,
                wall_ms: self.now_ms().saturating_sub(job.started_ms),
                cpu_usec: job.stats.cpu_usec,
                peak_mem_bytes: job.stats.mem_peak,
                io_read_bytes: job.stats.io_read,
                io_write_bytes: job.stats.io_write,
            };
            self.note_done(&job, &done);
        }
        let (phase, reason) = match (job.cancel, reported) {
            (Cancel::No, Some(_)) => (JobPhase::Exited, String::new()),
            (Cancel::No, None) => (
                JobPhase::Failed,
                "the job's scope ended without an exit status (killed outside crew, or lost with the machine)"
                    .to_string(),
            ),
            (_, _) => (JobPhase::Cancelled, "cancelled by `crew cancel`".to_string()),
        };
        let rec = self.running_record(&job, files);
        self.conclude(rec, phase, reported, reason);
    }

    /// Polls adopted jobs' scopes and escalates cancellations that SIGTERM did not end.
    fn watch_detached(&mut self) {
        let now = self.now_ms();
        for r in &mut self.running {
            match r.cancel {
                Cancel::Terminated { at_ms } if now.saturating_sub(at_ms) >= CANCEL_GRACE_MS => {
                    scope::kill(&r.scope_name, KillSignal::Kill);
                    r.cancel = Cancel::Killed;
                }
                // A cancel right after the start can beat the scope into existence, and then the signal went nowhere.
                Cancel::Terminated { .. } if !scope::alive(&r.scope_name) => {
                    scope::kill(&r.scope_name, KillSignal::Term);
                }
                _ => {}
            }
        }
        let gone: Vec<String> = self
            .running
            .iter()
            .filter(|r| {
                let Origin::Detached(files) = &r.origin else {
                    return false;
                };
                r.watch == Watch::Scope
                    && !scope::alive(&r.scope_name)
                    && (now.saturating_sub(r.started_ms) >= SCOPE_START_GRACE_MS
                        || scope::read_status(&files.status).is_some())
            })
            .map(|r| r.req.id.clone())
            .collect();
        for id in gone {
            self.end_detached(&id, None);
        }
    }

    fn cancel(&mut self, id: &str) -> Ack {
        let ack = |ok: bool, message: String| Ack { ok, message };
        if let Some(at) = self.queue.iter().position(|q| q.req.id == id) {
            let job = self.queue.remove(at);
            self.log.line(&format!("CANCEL id={id} while queued"));
            match &job.origin {
                Origin::Attached(conn) => {
                    self.remember(id, Fate::Refused);
                    conn.send(envelope::Body::Reply(Reply {
                        decision: Decision::Refused as i32,
                        outcome: Some(reply::Outcome::Refusal(Refusal {
                            reason: RefusalReason::Cancelled as i32,
                            detail: "cancelled with `crew cancel`".into(),
                        })),
                    }));
                }
                Origin::Detached(files) => self.conclude_unstarted(
                    &job,
                    files,
                    JobPhase::Cancelled,
                    "cancelled by `crew cancel` while queued".into(),
                ),
            }
            return ack(true, "cancelled while queued".into());
        }
        let now = self.now_ms();
        if let Some(r) = self.running.iter_mut().find(|r| r.req.id == id) {
            if r.cancel == Cancel::No {
                r.cancel = Cancel::Terminated { at_ms: now };
                scope::kill(&r.scope_name, KillSignal::Term);
                self.log
                    .line(&format!("CANCEL id={id} while running; SIGTERM sent"));
            }
            return ack(
                true,
                "cancelling: SIGTERM sent to the job's scope, SIGKILL follows in 10s".into(),
            );
        }
        ack(false, format!("no queued or running job {id}"))
    }

    fn jobs(&self) -> JobsReply {
        let queued = self.queue.iter().map(|q| {
            let (detached, log_path) = match &q.origin {
                Origin::Attached(_) => (false, String::new()),
                Origin::Detached(f) => (true, f.log.display().to_string()),
            };
            JobSummary {
                id: q.req.id.clone(),
                kind: q.kind as i32,
                agent: q.req.agent.clone(),
                project: q.req.project.clone(),
                command: command_of(&q.req),
                phase: JobPhase::Queued as i32,
                detached,
                exit_code: None,
                log_path,
                reason: q.detail.clone(),
                waiting_on: q.waiting as i32,
                position: q.position,
                queued_at_unix_ms: q.queued_at_ms,
                started_at_unix_ms: 0,
                finished_at_unix_ms: 0,
            }
        });
        let running = self.running.iter().map(|r| {
            let (detached, log_path) = match &r.origin {
                Origin::Attached(_) => (false, String::new()),
                Origin::Detached(f) => (true, f.log.display().to_string()),
            };
            JobSummary {
                id: r.req.id.clone(),
                kind: r.kind as i32,
                agent: r.req.agent.clone(),
                project: r.req.project.clone(),
                command: command_of(&r.req),
                phase: JobPhase::Running as i32,
                detached,
                exit_code: None,
                log_path,
                reason: String::new(),
                waiting_on: 0,
                position: 0,
                queued_at_unix_ms: r.queued_at_ms,
                started_at_unix_ms: r.started_ms,
                finished_at_unix_ms: 0,
            }
        });
        let finished = self.finished.iter().rev().map(detached::summary);
        JobsReply {
            jobs: queued.chain(running).chain(finished).collect(),
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
