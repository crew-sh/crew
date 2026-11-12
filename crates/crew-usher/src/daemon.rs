use std::collections::{BTreeMap, VecDeque};
use std::fs::{self, OpenOptions};
use std::io;
use std::os::unix::net::UnixStream;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crew_proto::{
    Ack, Decision, DetachedAccepted, DetachedJob, DiskView, Envelope, Grant, IdleWatcher, JobDone,
    JobKind, JobPhase, JobRequest, JobSummary, JobView, JobsReply, Limits, MachineView, OwnerSet,
    OwnerSource, Queued, Refusal, RefusalReason, Reply, TopReply, WaitCondition, WaitOutcome,
    WaitReply, WaitRequest, WaitingOn, envelope, reply,
};
use crew_sys::display::{self, Desktop};
use crew_sys::idle::IdleEvent;
use crew_sys::machine;
use crew_sys::procs;
use crew_sys::scope::{self, KillSignal, ScopeLimits};

use crate::admission::{self, Facts, Running, Verdict, Waiting};
use crate::agents::{self, Agent, Collector, Planner};
use crate::config::Config;
use crate::detached::{self, Dirs};
use crate::log::Log;
use crate::owner_state::{self, Manual};
use crate::pool::{self, PoolHold};
use crate::sizes::{self, Sizes};
use crate::state_file::Read;
use crate::waits::{self, Fate, JobState};

const TICK: Duration = Duration::from_secs(2);
const RENOTIFY_MS: u64 = 30_000;
const MIN_LEARNED_CORES_MILLI: u32 = 100;
const FATE_HISTORY: usize = 1024;
/// A client that lost the usher may claim its old queue time from a daemon that started this recently.
const RESUBMIT_GRACE: Duration = Duration::from_secs(10 * 60);
/// A temp dir younger than this may belong to a job whose scope is not up yet.
const TMP_SWEEP_MIN_AGE: Duration = Duration::from_secs(5 * 60);
/// The longest queue time a client of a pre-record usher may claim.
const LEGACY_CLAIM_MAX_MS: u64 = 24 * 3600 * 1000;
/// A job started this recently may not have its scope yet, so a missing scope proves nothing.
const SCOPE_START_GRACE_MS: u64 = 10_000;
/// A cancelled job gets SIGTERM, then SIGKILL after this long.
const CANCEL_GRACE_MS: u64 = 10_000;

#[derive(Clone)]
pub struct Conn {
    stream: Arc<Mutex<UnixStream>>,
    /// The client process, as the kernel saw it connect.
    pid: Option<u32>,
    /// Set by the connection's reader when the peer is gone.
    closed: Arc<AtomicBool>,
}

impl Conn {
    pub fn new(stream: UnixStream) -> io::Result<Self> {
        stream.set_write_timeout(Some(Duration::from_secs(2)))?;
        let pid = procs::peer_pid(&stream);
        Ok(Self {
            stream: Arc::new(Mutex::new(stream)),
            pid,
            closed: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn mark_closed(&self) {
        self.closed.store(true, Ordering::Relaxed);
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    pub fn same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.stream, &other.stream)
    }

    /// `pid (command)` of the client, for log lines.
    fn who(&self) -> String {
        match self.pid {
            Some(pid) => match procs::read_comm(Path::new("/proc"), pid) {
                Some(comm) => format!("pid {pid} ({comm})"),
                None => format!("pid {pid}"),
            },
            None => "an unknown process".to_string(),
        }
    }

    fn send(&self, body: envelope::Body) {
        let mut s = self
            .stream
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
        conn: Conn,
    },
    /// The connection that carried `job_id` closed.
    Gone {
        job_id: String,
        conn: Conn,
    },
    /// A detached job's process ended.
    Exited {
        job_id: String,
        exit: Exit,
    },
    Jobs {
        conn: Conn,
    },
    Cancel {
        job_id: String,
        conn: Conn,
    },
    /// Is the asking process inside this running job's scope?
    InsideJob {
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
    /// The idle watcher's subscription changed state.
    IdleWatcher(WatcherEvent),
    Stop,
}

/// How a detached job's `systemd-run` process ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    Code(i32),
    Signal(i32),
    /// It could not be waited for, or only its scope was watched.
    Unknown,
}

impl Exit {
    fn of(status: ExitStatus) -> Self {
        match (status.code(), status.signal()) {
            (Some(code), _) => Self::Code(code),
            (None, Some(sig)) => Self::Signal(sig),
            (None, None) => Self::Unknown,
        }
    }
}

/// What the idle watcher supervisor tells the actor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WatcherEvent {
    Watching,
    /// The compositor subscription ended after it had worked.
    Lost(String),
    /// A subscription could not be made.
    Unavailable(String),
}

/// Who owns a job: a `crew run` client on a connection, or the usher itself.
#[derive(Clone)]
enum Origin {
    Attached(Conn),
    /// An attached job whose client talked to an earlier usher; only its scope is known.
    Adopted,
    Detached(Files),
}

impl Origin {
    fn files(&self) -> Option<&Files> {
        match self {
            Self::Detached(f) => Some(f),
            Self::Attached(_) | Self::Adopted => None,
        }
    }
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
    /// The `crew run` process that holds an attached job: pid and start time.
    client: Option<(u32, u64)>,
    /// The scope has been seen to exist, so its absence now means the job is over.
    scope_seen: bool,
}

struct PendingWait {
    conn: Conn,
    condition: WaitCondition,
    job_id: String,
    since_ms: u64,
    deadline_ms: Option<u64>,
}

struct Owner {
    manual: Option<Manual>,
    idle: Option<bool>,
    watcher: IdleWatcher,
}

impl Default for Owner {
    fn default() -> Self {
        Self {
            manual: None,
            idle: None,
            watcher: IdleWatcher::NotConfigured,
        }
    }
}

impl Owner {
    /// Presence is the safe answer: without an override or an idle event, the owner is here.
    fn present(&self) -> bool {
        self.manual.map(|m| m.present).or(self.idle).unwrap_or(true)
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
    /// The manual owner override, kept across restarts.
    pub owner: PathBuf,
    pub jobs: PathBuf,
    pub job_logs: PathBuf,
}

pub struct Daemon {
    cfg: Config,
    sizes_path: PathBuf,
    owner_path: PathBuf,
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
    /// When the owner was last seen to leave; the drain clock of exclusive jobs starts there.
    owner_away_since_ms: Option<u64>,
    /// The exclusive job the last pass drained for, so the log notes when a drain starts.
    draining_for: Option<String>,
    load: CpuLoad,
    finished_cpu_usec: u64,
    waits: Vec<PendingWait>,
    fates: VecDeque<(String, Fate)>,
    agents: Option<AgentWatch>,
    planner: Planner,
    pool: PoolHold,
    /// Queued `crew run` clients of an earlier usher that may come back: id to first queue time.
    resubmits: BTreeMap<String, u64>,
    /// The state file came from a usher that did not record queued clients, so their claimed queue time is believed.
    legacy_claims: bool,
}

/// A queued job's entry goes to a new connection only when its client is gone, or the new connection is the same process.
fn may_reattach(old_closed: bool, old_pid: Option<u32>, new_pid: Option<u32>) -> bool {
    old_closed || (old_pid.is_some() && old_pid == new_pid)
}

/// The shape `crew` gives its ids (a v4 UUID); the temp sweep touches nothing else under a shared `tmp_root`.
fn looks_like_job_id(name: &str) -> bool {
    name.len() == 36
        && name.char_indices().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// Ids become unit names, file names and directory names, so they stay plain.
fn id_is_valid(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
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

/// Memory to reserve: a caller's own figure, else what earlier runs needed, held between half and eight times the kind's default.
fn expected_mem(kind_default: u64, declared: Option<u64>, learned: Option<u64>) -> u64 {
    if let Some(declared) = declared {
        return declared;
    }
    match learned.filter(|m| *m > 0) {
        Some(m) => m.clamp(kind_default / 2, kind_default.saturating_mul(8)),
        None => kind_default,
    }
}

/// A failed run, or one too short to have grown, says nothing about how big its command is.
fn worth_learning(done: &JobDone, min_wall_secs: u64) -> bool {
    done.exit_code == 0
        && done.peak_mem_bytes > 0
        && done.wall_ms >= min_wall_secs.saturating_mul(1000).max(1)
}

/// The scope is gone and either was seen before or had time to appear.
fn scope_over(r: &RunningJob, now_ms: u64) -> bool {
    !scope::alive(&r.scope_name)
        && (r.scope_seen || now_ms.saturating_sub(r.started_ms) >= SCOPE_START_GRACE_MS)
}

/// Why a job with no exit status failed; a failed `systemd-run` and a killed wrapper look alike from outside.
fn no_status_reason(exit: Exit) -> String {
    match exit {
        Exit::Code(c) => format!(
            "the job left no exit status; systemd-run exited {c} (it could not start the scope, or the job's wrapper was killed)"
        ),
        Exit::Signal(sig) => format!("the job left no exit status; systemd-run was killed by signal {sig}"),
        Exit::Unknown => "the job's scope ended without an exit status (killed outside crew, or lost with the machine)".to_string(),
    }
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
    /// Marks idle detection as configured but not yet connected; the supervisor reports the rest.
    #[must_use]
    pub fn expecting_idle_watcher(mut self) -> Self {
        self.owner.watcher = IdleWatcher::Unavailable;
        self
    }

    pub fn new(cfg: Config, paths: StatePaths, log: Arc<Log>, events: Sender<Msg>) -> Self {
        let boot_ms = unix_ms();
        let sizes = match Sizes::load(&paths.sizes, boot_ms) {
            Read::Loaded(s) => s,
            Read::Missing => Sizes::default(),
            Read::Unusable { why, to } => {
                log.line(&format!(
                    "ERROR {} is unusable ({why}); starting without learned sizes; kept as {}",
                    paths.sizes.display(),
                    to.map_or("(could not be moved)".into(), |t| t.display().to_string())
                ));
                Sizes::default()
            }
        };
        let sizes_path = paths.sizes;
        let owner_path = paths.owner;
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
            owner_path,
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
            owner_away_since_ms: None,
            draining_for: None,
            load: CpuLoad::default(),
            finished_cpu_usec: 0,
            waits: Vec::new(),
            fates: VecDeque::new(),
            agents,
            planner: Planner::default(),
            pool: PoolHold::default(),
            resubmits: BTreeMap::new(),
            legacy_claims: false,
        };
        daemon.restore_owner();
        daemon.restore();
        daemon.sweep_tmp();
        if let Err(e) = crew_sys::private::create_dir(Path::new(&daemon.cfg.tmp_root)) {
            daemon.log.line(&format!(
                "WARN tmp_root {} is not usable ({e}); jobs run without a private TMPDIR",
                daemon.cfg.tmp_root
            ));
        }
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
            Msg::Done { done, conn } => self.finish(&done, &conn),
            Msg::Gone { job_id, conn } => self.lose(&job_id, &conn),
            Msg::Exited { job_id, exit } => self.end_detached(&job_id, exit),
            Msg::Jobs { conn } => {
                let reply = self.jobs();
                conn.send(envelope::Body::JobsReply(reply));
            }
            Msg::Cancel { job_id, conn } => {
                let ack = self.cancel(&job_id);
                conn.send(envelope::Body::Ack(ack));
            }
            Msg::InsideJob { job_id, conn } => {
                let ack = self.inside_job(&job_id, &conn);
                conn.send(envelope::Body::Ack(ack));
            }
            Msg::Top { conn } => {
                let reply = self.top();
                conn.send(envelope::Body::TopReply(reply));
            }
            Msg::Owner { set, conn } => {
                let now = self.now_ms();
                match OwnerSource::try_from(set.source) {
                    Ok(OwnerSource::Auto) => {
                        self.log
                            .line(&format!("OWNER auto requested by {}", conn.who()));
                        self.set_owner(|o| o.manual = None);
                    }
                    Ok(OwnerSource::Manual) => {
                        self.log.line(&format!(
                            "OWNER {} requested by {}",
                            if set.present { "here" } else { "away" },
                            conn.who()
                        ));
                        self.set_owner(|o| {
                            o.manual = Some(Manual {
                                present: set.present,
                                since_ms: now,
                            });
                        });
                    }
                    _ => {
                        return Self::reject(
                            &conn,
                            "unknown owner source; use here, away or auto".into(),
                        );
                    }
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
            Msg::Idle(IdleEvent::Lost) => {
                self.watcher_event(WatcherEvent::Lost("the compositor connection ended".into()))
            }
            Msg::Idle(ev) => self.set_owner(|o| {
                o.idle = Some(ev == IdleEvent::Resumed);
                // Input after a manual `away` means the owner is back.
                if ev == IdleEvent::Resumed && o.manual.is_some_and(|m| !m.present) {
                    o.manual = None;
                }
            }),
            Msg::IdleWatcher(ev) => self.watcher_event(ev),
            Msg::Stop => {}
        }
    }

    /// A dead or missing watcher cannot say the owner left, so the owner counts as present until it reports again.
    fn watcher_event(&mut self, ev: WatcherEvent) {
        let (state, note) = match ev {
            WatcherEvent::Watching => {
                (IdleWatcher::Watching, "watching the compositor".to_string())
            }
            WatcherEvent::Lost(why) => (
                IdleWatcher::Lost,
                format!("lost ({why}); the owner counts as present until an idle event arrives"),
            ),
            WatcherEvent::Unavailable(why) => {
                (IdleWatcher::Unavailable, format!("unavailable ({why})"))
            }
        };
        let changed = self.owner.watcher != state;
        if changed {
            self.log.line(&format!("IDLE {note}"));
        }
        self.set_owner_quiet(|o| {
            o.watcher = state;
            if state != IdleWatcher::Watching {
                o.idle = None;
            }
        });
    }

    /// Drops a manual `away` that has outlived `manual_away_max_secs`.
    fn expire_manual_away(&mut self) {
        let now = self.now_ms();
        let max_ms = self.cfg.manual_away_max_secs.saturating_mul(1000);
        if self
            .owner
            .manual
            .is_some_and(|m| !m.present && now.saturating_sub(m.since_ms) >= max_ms)
        {
            self.log.line("OWNER manual away expired");
            self.set_owner(|o| o.manual = None);
        }
    }

    fn set_owner(&mut self, change: impl FnOnce(&mut Owner)) {
        self.set_owner_with(change, true);
    }

    fn set_owner_quiet(&mut self, change: impl FnOnce(&mut Owner)) {
        self.set_owner_with(change, false);
    }

    /// Takes back a manual override an earlier usher kept; an `away` that outlived its maximum meanwhile is dropped.
    fn restore_owner(&mut self) {
        let manual = match owner_state::load(&self.owner_path) {
            Read::Loaded(m) => m,
            Read::Missing => return,
            Read::Unusable { why, to } => {
                self.log.line(&format!(
                    "ERROR {} is unusable ({why}); the owner is unset; kept as {}",
                    self.owner_path.display(),
                    to.map_or("(could not be moved)".into(), |t| t.display().to_string())
                ));
                return;
            }
        };
        let max_ms = self.cfg.manual_away_max_secs.saturating_mul(1000);
        if manual.expired(self.now_ms(), max_ms) {
            self.log.line("OWNER manual away expired while the usher was down");
            self.save_owner();
            return;
        }
        self.log.line(&format!(
            "OWNER restored manual {}",
            if manual.present { "here" } else { "away" }
        ));
        self.owner.manual = Some(manual);
    }

    fn save_owner(&self) {
        if let Err(e) = owner_state::store(&self.owner_path, self.owner.manual) {
            self.log
                .line(&format!("WARN could not save the owner state: {e}"));
        }
    }

    fn set_owner_with(&mut self, change: impl FnOnce(&mut Owner), always_log: bool) {
        let before = (self.owner.present(), self.owner.source());
        let manual_before = self.owner.manual;
        change(&mut self.owner);
        if self.owner.manual != manual_before {
            self.save_owner();
        }
        let now = self.owner.present();
        if always_log || before != (now, self.owner.source()) {
            self.log.line(&format!(
                "OWNER present={now} source={}",
                source_name(self.owner.source())
            ));
        }
        if before.0 != now {
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
        let mem = expected_mem(
            spec.expected_mem,
            declared.mem_bytes,
            learned.map(|l| l.peak_mem_bytes),
        );
        // What the caller asked for beats what earlier runs suggested.
        let cores = declared
            .cpus
            .map(f64::from)
            .or_else(|| {
                learned.map(|l| f64::from(l.cores_milli.max(MIN_LEARNED_CORES_MILLI)) / 1000.0)
            })
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
        if !id_is_valid(&req.id) {
            return Self::reject(
                &conn,
                "a job id is 1 to 64 letters, digits and dashes".into(),
            );
        }
        if self.running.iter().any(|r| r.req.id == req.id) {
            conn.send(envelope::Body::Ack(Ack {
                ok: false,
                message: format!("job {} is already running", req.id),
            }));
            return;
        }
        // The same client coming back after losing its connection keeps its place; nobody else can take the entry over.
        if let Some(q) = self.queue.iter_mut().find(|q| q.req.id == req.id) {
            let owner_left = match &q.origin {
                Origin::Attached(old) => may_reattach(old.is_closed(), old.pid, conn.pid),
                Origin::Adopted | Origin::Detached(_) => false,
            };
            if !owner_left {
                return Self::reject(&conn, format!("job {} is queued by another client", req.id));
            }
            q.origin = Origin::Attached(conn);
            q.last_notice_ms = None;
            return;
        }
        let now = self.now_ms();
        let at = self.resubmit_time(&req, now);
        if at < now {
            self.log.line(&format!(
                "RESUBMIT id={} queued {}s ago",
                req.id,
                (now - at) / 1000
            ));
        }
        let job = self.queued_job(req, Origin::Attached(conn), at);
        self.insert_queued(job);
        self.persist();
    }

    /// When a resubmitting client first queued: what an earlier usher recorded, never a client's own say-so.
    fn resubmit_time(&mut self, req: &JobRequest, now: u64) -> u64 {
        if let Some(recorded) = self.resubmits.remove(&req.id) {
            return recorded.min(now);
        }
        // A usher that predates the record keeps believing its clients, within bounds, for a short while.
        if self.legacy_claims
            && self.started.elapsed() < RESUBMIT_GRACE
            && req.queued_at_unix_ms > 0
        {
            return req
                .queued_at_unix_ms
                .clamp(now.saturating_sub(LEGACY_CLAIM_MAX_MS), now);
        }
        now
    }

    fn reject(conn: &Conn, message: String) {
        conn.send(envelope::Body::Ack(Ack { ok: false, message }));
    }

    fn enqueue_detached(&mut self, mut req: JobRequest, conn: &Conn) {
        if !id_is_valid(&req.id) || req.argv.is_empty() {
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
        let (mut free_disk, mut unmeasurable_disk) = (Vec::new(), Vec::new());
        for path in &self.cfg.watched_paths {
            match machine::disk_free(path) {
                Ok(free) => free_disk.push((path.clone(), free)),
                Err(_) => unmeasurable_disk.push(path.clone()),
            }
        }
        Facts {
            mem_total: snap.mem_total,
            unmeasurable_disk,
            mem_available: snap.mem_available,
            cpu_count: snap.cpu_count.max(1),
            unmanaged_cores: self.load.unmanaged_cores,
            free_disk,
            owner_present: self.owner.present(),
            owner_away_since_ms: (!self.owner.present())
                .then(|| self.owner_away_since_ms.unwrap_or_else(|| self.now_ms())),
            now_ms: self.now_ms(),
        }
    }

    fn track_owner(&mut self) {
        if self.owner.present() {
            self.owner_away_since_ms = None;
        } else if self.owner_away_since_ms.is_none() {
            self.owner_away_since_ms = Some(self.now_ms());
        }
    }

    fn learned_wall(&self, req: &JobRequest) -> Option<u64> {
        self.sizes
            .expected(&sizes::key(req))
            .and_then(|l| l.wall_ms)
    }

    /// The running jobs and the queue as admission sees them.
    fn admission_inputs(&self) -> (Vec<Running>, Vec<Waiting>) {
        let running = self
            .running
            .iter()
            .map(|r| Running {
                label: label_of(&r.req, r.kind),
                kind: r.kind,
                expected_mem: r.expected_mem,
                expected_cores: r.expected_cores,
                observed_peak: r.stats.peak_worth_learning(),
                exclusive: r.req.exclusive,
                ends_at_ms: self.learned_wall(&r.req).map(|w| r.started_ms + w),
            })
            .collect();
        let waiting = self
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
                exclusive: q.req.exclusive,
                expected_wall_ms: self.learned_wall(&q.req),
            })
            .collect();
        (running, waiting)
    }

    /// When each queued job should start, in ms from now, and the exclusive job the machine drains for.
    fn outlook(&self) -> (BTreeMap<String, u64>, Option<String>) {
        if self.queue.is_empty() {
            return (BTreeMap::new(), None);
        }
        let facts = self.facts();
        let (running, waiting) = self.admission_inputs();
        let draining_for = admission::schedule(&self.cfg, &facts, &running, &waiting).draining_for;
        (
            admission::starts(&self.cfg, &facts, &running, &waiting),
            draining_for,
        )
    }

    fn reschedule(&mut self) {
        self.track_owner();
        if self.queue.is_empty() {
            return;
        }
        let facts = self.facts();
        let (running, waiting) = self.admission_inputs();
        let plan = admission::schedule(&self.cfg, &facts, &running, &waiting);
        if plan.draining_for.is_some() && plan.draining_for != self.draining_for {
            self.log.line(&format!(
                "DRAIN for {}",
                plan.draining_for.as_deref().unwrap_or_default()
            ));
        }
        self.draining_for = plan.draining_for;
        let verdicts = plan.verdicts;
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
                        Origin::Adopted => {}
                        Origin::Detached(_) => self.conclude_unstarted(
                            &job,
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
        let id = &job.req.id;
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
            scope_seen: false,
            client: match &origin {
                Origin::Attached(c) => c.pid.and_then(|pid| {
                    procs::read_start_ticks(Path::new("/proc"), pid).map(|t| (pid, t))
                }),
                Origin::Adopted | Origin::Detached(_) => None,
            },
        });
        let at = self.running.len() - 1;
        match origin {
            Origin::Attached(conn) => {
                self.persist();
                conn.send(envelope::Body::Reply(Reply {
                    decision: Decision::Granted as i32,
                    outcome: Some(reply::Outcome::Grant(grant)),
                }));
            }
            Origin::Adopted => {}
            Origin::Detached(files) => match self.launch(&self.running[at], &grant, &files) {
                Ok(tmpdir) => {
                    self.running[at].tmpdir = tmpdir;
                    self.persist();
                }
                Err(e) => {
                    let job = self.running.remove(at);
                    self.log
                        .line(&format!("FAIL id={} could not start: {e}", job.req.id));
                    let rec = DetachedJob {
                        started_at_unix_ms: 0,
                        ..self.running_record(&job)
                    };
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
        let tmpdir = tmpdir.filter(|dir| crew_sys::private::create_dir(dir).is_ok());
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
                let exit = child.wait().map_or(Exit::Unknown, Exit::of);
                let _ = tx.send(Msg::Exited { job_id: id, exit });
            })?;
        Ok(tmpdir)
    }

    /// Books a finished job's usage and sizes, and logs it.
    fn note_done(&mut self, job: &RunningJob, done: &JobDone) {
        self.finished_cpu_usec += done.cpu_usec;
        if worth_learning(done, self.cfg.learn_min_wall_secs) {
            let cores_milli = u32::try_from(done.cpu_usec / done.wall_ms).unwrap_or(u32::MAX);
            self.sizes.record(
                &sizes::key(&job.req),
                sizes::Learned {
                    peak_mem_bytes: done.peak_mem_bytes,
                    cores_milli,
                    wall_ms: Some(done.wall_ms),
                },
                self.boot_unix_ms + self.started.elapsed().as_millis() as u64,
            );
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

    /// Only the connection that was granted the job may end it.
    fn finish(&mut self, done: &JobDone, from: &Conn) {
        let Some(at) = self.running.iter().position(|r| {
            r.req.id == done.job_id && matches!(&r.origin, Origin::Attached(c) if c.same(from))
        }) else {
            self.log.line(&format!(
                "IGNORE JobDone for {} from {}: not the job's client",
                done.job_id,
                from.who()
            ));
            return;
        };
        let job = self.running.remove(at);
        self.note_done(&job, done);
        self.stop_leftovers(&job.scope_name);
        self.persist();
        if let Origin::Attached(conn) = &job.origin {
            conn.send(envelope::Body::Ack(Ack {
                ok: true,
                message: String::new(),
            }));
        }
    }

    /// A job's processes do not outlive the job: whatever its command left in the scope goes with it.
    fn stop_leftovers(&self, scope_name: &str) {
        if scope::alive(scope_name) {
            scope::stop(scope_name);
            self.log.line(&format!(
                "STOP scope={scope_name} still had processes after the job ended"
            ));
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
            self.persist();
        } else if let Some(at) = self
            .running
            .iter()
            .position(|r| r.req.id == job_id && ours(&r.origin))
        {
            let job = self.running.remove(at);
            scope::kill(&job.scope_name, KillSignal::Kill);
            self.log
                .line(&format!("GONE id={job_id} while running; scope killed"));
            let _ = fs::remove_dir_all(self.job_tmpdir(job_id));
            self.persist();
        }
    }

    fn tick(&mut self) {
        for r in &mut self.running {
            if let Some(s) = scope::stats(&r.scope_name) {
                r.stats.observe(s);
                r.scope_seen = true;
            }
        }
        for line in self.pool.poll() {
            self.log.line(&line);
        }
        self.sync_pool();
        self.update_load();
        self.expire_manual_away();
        self.expire_resubmits();
        self.reap_orphans();
        self.watch_detached();
        self.reschedule();
        self.adopt_dry_run();
        self.settle_waits();
    }

    fn job_tmpdir(&self, id: &str) -> PathBuf {
        Path::new(self.cfg.tmp_root.trim_end_matches('/')).join(id)
    }

    /// Removes job temp dirs that no scope and no known job accounts for: a killed client leaves its dir behind.
    fn sweep_tmp(&self) {
        let live = scope::live_scopes();
        let known: Vec<&Path> = self
            .running
            .iter()
            .filter_map(|r| r.tmpdir.as_deref())
            .collect();
        let Ok(entries) = fs::read_dir(&self.cfg.tmp_root) else {
            return;
        };
        let mut removed = 0;
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            let recent = entry
                .metadata()
                .and_then(|m| m.modified())
                .is_ok_and(|t| t.elapsed().unwrap_or_default() < TMP_SWEEP_MIN_AGE);
            let owned = live.iter().any(|s| s.ends_with(&format!("-{name}")));
            if entry.file_type().is_ok_and(|t| t.is_dir())
                && looks_like_job_id(&name)
                && !recent
                && !owned
                && !known.contains(&path.as_path())
                && fs::remove_dir_all(&path).is_ok()
            {
                removed += 1;
            }
        }
        if removed > 0 {
            self.log.line(&format!(
                "SWEEP removed {removed} leftover job temp dir(s) under {}",
                self.cfg.tmp_root
            ));
        }
    }

    /// An adopted `crew run` job whose client died while no usher watched has nobody to stop it: stop its scope.
    fn reap_orphans(&mut self) {
        for r in &self.running {
            let (Origin::Adopted, Some((pid, ticks))) = (&r.origin, r.client) else {
                continue;
            };
            if procs::read_start_ticks(Path::new("/proc"), pid) != Some(ticks) {
                scope::kill(&r.scope_name, KillSignal::Kill);
                self.log.line(&format!(
                    "ORPHAN id={} its client (pid {pid}) is gone; scope killed",
                    r.req.id
                ));
            }
        }
    }

    /// Forgets queued clients of the earlier usher that never came back.
    fn expire_resubmits(&mut self) {
        if !self.resubmits.is_empty() && self.started.elapsed() >= RESUBMIT_GRACE {
            self.log.line(&format!(
                "RESUBMIT window closed; {} never came back",
                self.resubmits.len()
            ));
            self.resubmits.clear();
            self.persist();
        }
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
                Origin::Attached(_) | Origin::Adopted => JobState::Queued,
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
        if self.running.iter().any(|r| r.req.id == id) {
            return JobState::Granted;
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
        let (starts, draining_for) = self.outlook();
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
            ..JobView::default()
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
            .map(|q| JobView {
                starts_in_ms: starts.get(&q.req.id).copied(),
                detail: q.detail.clone(),
                ..view(
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
                idle_watcher: self.owner.watcher as i32,
                unmanaged_cpu_pct: self.load.unmanaged_cores / f64::from(snap.cpu_count.max(1))
                    * 100.0,
                draining_for: draining_for.unwrap_or_default(),
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
        files: Option<&Files>,
        queued_at_ms: u64,
        phase: JobPhase,
    ) -> DetachedJob {
        DetachedJob {
            request: Some(req.clone()),
            phase: phase as i32,
            queued_at_unix_ms: queued_at_ms,
            log_path: files.map_or_else(String::new, |f| f.log.display().to_string()),
            status_path: files.map_or_else(String::new, |f| f.status.display().to_string()),
            attached: files.is_none(),
            ..DetachedJob::default()
        }
    }

    fn queued_record(&self, q: &QueuedJob) -> DetachedJob {
        DetachedJob {
            expected_mem_bytes: q.expected_mem,
            expected_cores_milli: (q.expected_cores * 1000.0) as u32,
            ..self.base_record(&q.req, q.origin.files(), q.queued_at_ms, JobPhase::Queued)
        }
    }

    fn running_record(&self, r: &RunningJob) -> DetachedJob {
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
            client_pid: r.client.map_or(0, |c| c.0),
            client_start_ticks: r.client.map_or(0, |c| c.1),
            ..self.base_record(&r.req, r.origin.files(), r.queued_at_ms, JobPhase::Running)
        }
    }

    /// Writes what must outlive the usher: detached jobs, running and queued `crew run` jobs.
    fn persist(&self) {
        let mut jobs = self.finished.clone();
        jobs.extend(self.running.iter().map(|r| self.running_record(r)));
        jobs.extend(self.queue.iter().map(|q| self.queued_record(q)));
        for (id, queued_at) in &self.resubmits {
            jobs.push(DetachedJob {
                request: Some(JobRequest {
                    id: id.clone(),
                    ..JobRequest::default()
                }),
                phase: JobPhase::Queued as i32,
                queued_at_unix_ms: *queued_at,
                attached: true,
                ..DetachedJob::default()
            });
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
    fn conclude_unstarted(&mut self, job: &QueuedJob, phase: JobPhase, reason: String) {
        let rec = self.queued_record(job);
        self.conclude(rec, phase, None, reason);
    }

    /// Takes back what an earlier usher left; running jobs keep their reservations until their scopes end.
    fn restore(&mut self) {
        let stored = match detached::load(&self.jobs_path) {
            Read::Loaded(stored) => stored,
            Read::Missing => return,
            Read::Unusable { why, to } => {
                self.log.line(&format!(
                    "ERROR {} is unusable ({why}); starting with an empty queue; the file is kept as {}",
                    self.jobs_path.display(),
                    to.map_or("(it could not be moved)".into(), |t| t.display().to_string())
                ));
                return;
            }
        };
        self.legacy_claims = stored.version < 2;
        let (mut queued, mut adopted, mut ended, mut waiting) = (0, 0, 0, 0);
        for rec in stored.jobs {
            let Some(req) = rec.request.clone() else {
                continue;
            };
            let phase = detached::phase(&rec);
            if rec.attached {
                match phase {
                    JobPhase::Queued => {
                        self.resubmits.insert(req.id.clone(), rec.queued_at_unix_ms);
                        waiting += 1;
                    }
                    JobPhase::Running if self.scope_still_there(&rec, None) => {
                        self.running
                            .push(self.adopted_job(req, &rec, Origin::Adopted));
                        adopted += 1;
                    }
                    _ => {
                        // Its client is gone with the scope; only the job's private temp dir can be left.
                        if !rec.tmpdir.is_empty() {
                            let _ = fs::remove_dir_all(&rec.tmpdir);
                        }
                    }
                }
                continue;
            }
            let files = Files {
                log: PathBuf::from(&rec.log_path),
                status: PathBuf::from(&rec.status_path),
            };
            match phase {
                JobPhase::Queued => {
                    let job = self.queued_job(req, Origin::Detached(files), rec.queued_at_unix_ms);
                    self.queue.push(job);
                    queued += 1;
                }
                JobPhase::Running if self.scope_still_there(&rec, Some(&files)) => {
                    self.running
                        .push(self.adopted_job(req, &rec, Origin::Detached(files)));
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
            "RESTORE queued={queued} adopted={adopted} ended_while_down={ended} finished={} clients_expected={waiting}",
            self.finished.len()
        ));
        self.persist();
    }

    /// Whether a record's job may still be running: its scope exists, or it started too recently to have one.
    fn scope_still_there(&self, rec: &DetachedJob, files: Option<&Files>) -> bool {
        scope::alive(&rec.scope_name)
            || (self.now_ms().saturating_sub(rec.started_at_unix_ms) < SCOPE_START_GRACE_MS
                && files.is_none_or(|f| scope::read_status(&f.status).is_none()))
    }

    fn adopted_job(&self, req: JobRequest, rec: &DetachedJob, origin: Origin) -> RunningJob {
        RunningJob {
            kind: JobKind::try_from(req.kind).unwrap_or(JobKind::Unspecified),
            req,
            origin,
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
            client: (rec.client_pid > 0).then_some((rec.client_pid, rec.client_start_ticks)),
            scope_seen: scope::alive(&rec.scope_name),
        }
    }

    /// A detached job's process ended: record how, free its slot, and learn its size.
    fn end_detached(&mut self, id: &str, exit: Exit) {
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
        self.stop_leftovers(&job.scope_name);
        // The wrapper's status file is the job's own word; the process exit alone cannot tell the job from a failed systemd-run.
        let reported = scope::read_status(&files.status);
        if let Some(exit_code) = reported {
            let done = JobDone {
                job_id: id.to_string(),
                exit_code,
                wall_ms: self.now_ms().saturating_sub(job.started_ms),
                cpu_usec: job.stats.cpu_usec,
                peak_mem_bytes: job.stats.peak_worth_learning(),
                working_set: true,
                io_read_bytes: job.stats.io_read,
                io_write_bytes: job.stats.io_write,
            };
            self.note_done(&job, &done);
        }
        let (phase, reason) = match (job.cancel, reported) {
            (Cancel::No, Some(_)) => (JobPhase::Exited, String::new()),
            (Cancel::No, None) => (JobPhase::Failed, no_status_reason(exit)),
            (_, _) => (
                JobPhase::Cancelled,
                "cancelled by `crew cancel`".to_string(),
            ),
        };
        let rec = self.running_record(&job);
        self.conclude(rec, phase, reported, reason);
    }

    /// An adopted `crew run` job whose scope is gone: free its reservation.
    fn end_adopted(&mut self, id: &str) {
        if let Some(at) = self
            .running
            .iter()
            .position(|r| r.req.id == id && matches!(r.origin, Origin::Adopted))
        {
            let job = self.running.remove(at);
            let _ = fs::remove_dir_all(self.job_tmpdir(&job.req.id));
            self.log
                .line(&format!("END id={id} adopted job's scope is gone"));
            self.persist();
        }
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
        let gone: Vec<(String, bool)> = self
            .running
            .iter()
            .filter(|r| r.watch == Watch::Scope)
            .filter(|r| match &r.origin {
                // The status file is the end of the job even if a child it left behind keeps the scope alive.
                Origin::Detached(files) => {
                    scope::read_status(&files.status).is_some() || scope_over(r, now)
                }
                Origin::Adopted => scope_over(r, now),
                Origin::Attached(_) => false,
            })
            .map(|r| (r.req.id.clone(), matches!(r.origin, Origin::Adopted)))
            .collect();
        for (id, adopted) in gone {
            if adopted {
                self.end_adopted(&id);
            } else {
                self.end_detached(&id, Exit::Unknown);
            }
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
                Origin::Adopted => {}
                Origin::Detached(_) => self.conclude_unstarted(
                    &job,
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

    /// Whether `conn`'s process sits in the cgroup of the running job `id`; `CREW_JOB` alone proves nothing.
    fn inside_job(&self, id: &str, conn: &Conn) -> Ack {
        let verdict = |ok: bool, message: &str| Ack {
            ok,
            message: message.to_string(),
        };
        let Some(job) = self.running.iter().find(|r| r.req.id == id) else {
            return verdict(false, "no such job is running");
        };
        let Some(pid) = conn.pid else {
            return verdict(false, "cannot tell which process asks");
        };
        let unit = format!("{}.scope", job.scope_name);
        let inside = procs::read_cgroup_path(Path::new("/proc"), pid)
            .is_some_and(|path| path.split('/').any(|part| part == unit));
        if inside {
            verdict(true, "")
        } else {
            verdict(false, "the caller is not inside the job's scope")
        }
    }

    fn jobs(&self) -> JobsReply {
        let (starts, _) = self.outlook();
        let queued = self.queue.iter().map(|q| {
            let (detached, log_path) = match &q.origin {
                Origin::Attached(_) | Origin::Adopted => (false, String::new()),
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
                starts_in_ms: starts.get(&q.req.id).copied(),
            }
        });
        let running = self.running.iter().map(|r| {
            let (detached, log_path) = match &r.origin {
                Origin::Attached(_) | Origin::Adopted => (false, String::new()),
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
                starts_in_ms: None,
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

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    #[test]
    fn only_the_owning_process_or_a_gone_one_may_be_replaced() {
        assert!(
            may_reattach(true, Some(10), Some(20)),
            "the old client is gone"
        );
        assert!(
            may_reattach(false, Some(10), Some(10)),
            "the same process came back"
        );
        assert!(
            !may_reattach(false, Some(10), Some(20)),
            "another live process"
        );
        assert!(!may_reattach(false, None, None), "nobody can be identified");
    }

    #[test]
    fn only_uuid_shaped_names_are_job_dirs() {
        assert!(looks_like_job_id("5cbd0f7a-0000-4000-8000-0000000000aa"));
        for not in [
            "",
            "not-a-job",
            "5cbd0f7a-0000-4000-8000-0000000000a",
            "5cbd0f7a_0000_4000_8000_0000000000aa",
            "../5cbd0f7a-0000-4000-8000-00000000",
        ] {
            assert!(!looks_like_job_id(not), "{not}");
        }
    }

    #[test]
    fn a_declared_size_beats_a_learned_one() {
        assert_eq!(
            expected_mem(2 * GIB, Some(40 * GIB), Some(3 * GIB)),
            40 * GIB
        );
        assert_eq!(expected_mem(2 * GIB, None, Some(3 * GIB)), 3 * GIB);
        assert_eq!(expected_mem(2 * GIB, None, None), 2 * GIB);
    }

    #[test]
    fn a_learned_size_stays_near_the_kinds_default() {
        // 20 no-op builds of 200 MiB must not become the reservation for a cold build.
        assert_eq!(expected_mem(2 * GIB, None, Some(200 << 20)), GIB);
        // Cache charged to the job once taught 40 GiB; the cap keeps that from sticking.
        assert_eq!(expected_mem(2 * GIB, None, Some(40 * GIB)), 16 * GIB);
    }

    fn done(exit_code: i32, wall_ms: u64, peak: u64) -> JobDone {
        JobDone {
            exit_code,
            wall_ms,
            peak_mem_bytes: peak,
            ..JobDone::default()
        }
    }

    #[test]
    fn failed_and_very_short_runs_teach_nothing() {
        assert!(worth_learning(&done(0, 60_000, GIB), 5));
        assert!(!worth_learning(&done(101, 60_000, GIB), 5));
        assert!(!worth_learning(&done(0, 800, GIB), 5));
        assert!(!worth_learning(&done(0, 60_000, 0), 5));
    }
}
