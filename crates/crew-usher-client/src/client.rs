use std::collections::BTreeMap;
use std::os::unix::net::UnixStream;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crew_proto::envelope::Body;
use crew_proto::{
    Ack, CancelRequest, Envelope, FrameError, Grant, JobDone, JobKind, JobRequest, JobsReply,
    JobsRequest, OwnerSet, OwnerSource, Reply, TopReply, TopRequest, WaitCondition, WaitReply,
    WaitRequest, WaitingOn, reply,
};
use crew_sys::display::{self, Desktop};
use crew_sys::scope::{self, KillSignal, ScopeLimits, Stats};
use crew_sys::signals;

use crate::error::ClientError;
use crate::json::{self, RunEvent, UnmanagedReason};
use crate::kind;
use crate::request::{self, Inputs};

const FIRST_POLL: Duration = Duration::from_millis(20);
const MAX_TICK: Duration = Duration::from_millis(100);
const STATS_EVERY: Duration = Duration::from_millis(500);
const FAST_POLL_WINDOW: Duration = Duration::from_secs(2);
/// How long a queued client waits for a lost usher to return before it gives up (never by running unmanaged).
pub const RECONNECT_BUDGET: Duration = Duration::from_secs(5 * 60);
const RECONNECT_FIRST: Duration = Duration::from_millis(200);
const RECONNECT_MAX: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RunOptions {
    pub kind: Option<JobKind>,
    pub mem_bytes: Option<u64>,
    pub cpus: Option<u32>,
    pub display: bool,
    pub argv: Vec<String>,
    /// A missing daemon is an error instead of an unmanaged run.
    pub strict: bool,
    /// Progress as JSON lines on stderr instead of text.
    pub json: bool,
    /// The caller already runs inside a granted job (CREW_JOB): run in its scope instead of asking again.
    pub inside_job: bool,
    /// With `Client::detach`: where the job's output goes; default is a file in the usher's state directory.
    pub log: Option<PathBuf>,
}

/// A job the usher now owns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Detached {
    pub job_id: String,
    pub log_path: PathBuf,
}

/// What a caller changes about the command once it knows whether it holds a grant.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Launch {
    /// Runs instead of `RunOptions::argv`; the usher still sees `argv`, so it sizes the job by what the caller asked for.
    pub argv: Option<Vec<String>>,
    pub env_set: BTreeMap<String, String>,
    pub env_unset: Vec<String>,
}

impl Launch {
    fn apply(self, argv: &[String], env: &mut BTreeMap<String, String>) -> Vec<String> {
        for k in &self.env_unset {
            env.remove(k);
        }
        env.extend(self.env_set);
        self.argv.unwrap_or_else(|| argv.to_vec())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WaitSpec {
    pub condition: WaitCondition,
    pub job_id: String,
    pub timeout: Option<Duration>,
}

/// Longer than any answer the usher owes, so a dead usher cannot hang a wait forever.
const WAIT_GRACE: Duration = Duration::from_secs(15);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OwnerArg {
    Here,
    Away,
    Auto,
}

pub struct Client {
    socket: PathBuf,
}

fn exit_code(status: ExitStatus) -> i32 {
    status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
}

fn limits_of(grant: &Grant) -> ScopeLimits {
    let l = grant.limits.unwrap_or_default();
    ScopeLimits {
        mem_bytes: l.mem_bytes,
        cpu_weight: l.cpu_weight,
        io_weight: l.io_weight,
        cpus: l.cpus,
    }
}

fn describe_wait(q: &crew_proto::Queued) -> String {
    let waiting = WaitingOn::try_from(q.waiting_on).unwrap_or(WaitingOn::Unspecified);
    format!(
        "crew: queued #{} for {}s, waiting on {}: {}",
        q.position,
        q.queued_ms / 1000,
        waiting.name(),
        q.detail
    )
}

impl Client {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
        }
    }

    pub fn from_env() -> Self {
        Self::new(crew_sys::paths::socket_path())
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    fn connect(&self) -> Result<UnixStream, ClientError> {
        UnixStream::connect(&self.socket).map_err(|source| ClientError::DaemonDown {
            socket: self.socket.display().to_string(),
            source,
        })
    }

    fn ask(&self, body: Body) -> Result<Envelope, ClientError> {
        let mut s = self.connect()?;
        crew_proto::write_frame(&mut s, &Envelope::of(body))?;
        crew_proto::read_frame(&mut s)?.ok_or(ClientError::Closed)
    }

    pub fn top(&self) -> Result<TopReply, ClientError> {
        match self.ask(Body::TopRequest(TopRequest {}))?.body {
            Some(Body::TopReply(t)) => Ok(t),
            _ => Err(ClientError::Protocol),
        }
    }

    pub fn set_owner(&self, arg: OwnerArg) -> Result<String, ClientError> {
        let (present, source) = match arg {
            OwnerArg::Here => (true, OwnerSource::Manual),
            OwnerArg::Away => (false, OwnerSource::Manual),
            OwnerArg::Auto => (false, OwnerSource::Auto),
        };
        match self
            .ask(Body::OwnerSet(OwnerSet {
                present,
                source: source as i32,
            }))?
            .body
        {
            Some(Body::Ack(a)) => Ok(a.message),
            _ => Err(ClientError::Protocol),
        }
    }

    /// Blocks until the usher reports the condition (it notifies; nothing is polled).
    pub fn wait(&self, spec: &WaitSpec) -> Result<WaitReply, ClientError> {
        let mut s = self.connect()?;
        let req = WaitRequest {
            condition: spec.condition as i32,
            job_id: spec.job_id.clone(),
            timeout_ms: spec.timeout.map_or(0, |t| t.as_millis().max(1) as u64),
        };
        crew_proto::write_frame(&mut s, &Envelope::of(Body::WaitRequest(req)))?;
        if let Some(t) = spec.timeout {
            s.set_read_timeout(Some(t + WAIT_GRACE))?;
        }
        match crew_proto::read_frame(&mut s)?
            .ok_or(ClientError::Closed)?
            .body
        {
            Some(Body::WaitReply(w)) => Ok(w),
            _ => Err(ClientError::Protocol),
        }
    }

    pub fn jobs(&self) -> Result<JobsReply, ClientError> {
        match self.ask(Body::JobsRequest(JobsRequest {}))?.body {
            Some(Body::JobsReply(r)) => Ok(r),
            _ => Err(ClientError::Protocol),
        }
    }

    pub fn cancel(&self, job_id: &str) -> Result<Ack, ClientError> {
        let req = CancelRequest {
            job_id: job_id.to_string(),
        };
        match self.ask(Body::CancelRequest(req))?.body {
            Some(Body::Ack(a)) => Ok(a),
            _ => Err(ClientError::Protocol),
        }
    }

    /// Hands the command to the usher, which queues it, runs it itself and keeps its output in a log; the caller can exit.
    pub fn detach(&self, opts: &RunOptions) -> Result<Detached, ClientError> {
        if opts.argv.is_empty() {
            return Err(ClientError::EmptyCommand);
        }
        let mut env: BTreeMap<String, String> = std::env::vars().collect();
        // The job gets its own CREW_JOB from the usher.
        env.remove("CREW_JOB");
        let cwd = std::env::current_dir()?;
        let kind = match opts.kind {
            Some(k) => k,
            None => kind::infer_with(&opts.argv, &kind::ProjectRules::discover(&cwd)?),
        };
        let mut req = request::build(&Inputs {
            kind,
            argv: &opts.argv,
            cwd: &cwd,
            env: &env,
            mem_bytes: opts.mem_bytes,
            cpus: opts.cpus,
            display: opts.display,
        });
        req.detach = true;
        req.env = env.into_iter().collect();
        req.log_path = opts
            .log
            .as_ref()
            .map(|p| cwd.join(p).display().to_string())
            .unwrap_or_default();
        let mut stream = self.connect()?;
        crew_proto::write_frame(&mut stream, &Envelope::of(Body::JobRequest(req)))?;
        match crew_proto::read_frame(&mut stream)?
            .ok_or(ClientError::Closed)?
            .body
        {
            Some(Body::DetachedAccepted(a)) => Ok(Detached {
                job_id: a.job_id,
                log_path: PathBuf::from(a.log_path),
            }),
            Some(Body::Ack(a)) if !a.ok => Err(ClientError::Rejected(a.message)),
            _ => Err(ClientError::Protocol),
        }
    }

    /// Waits for a lost usher to come back and queues `req` again under its old id and time.
    fn requeue(
        &self,
        req: &JobRequest,
        queued_at_unix_ms: u64,
        opts: &RunOptions,
    ) -> Result<UnixStream, ClientError> {
        if opts.json {
            progress(
                opts,
                &RunEvent::UsherLost {
                    job_id: req.id.clone(),
                },
            );
        } else {
            eprintln!(
                "crew: lost the usher; job {} stays queued, waiting for it to come back",
                req.id
            );
        }
        let started = Instant::now();
        let mut delay = RECONNECT_FIRST;
        loop {
            thread::sleep(delay);
            delay = (delay * 2).min(RECONNECT_MAX);
            let source = match UnixStream::connect(&self.socket) {
                Ok(mut s) => {
                    let again = JobRequest {
                        queued_at_unix_ms,
                        ..req.clone()
                    };
                    match crew_proto::write_frame(&mut s, &Envelope::of(Body::JobRequest(again))) {
                        Ok(()) => {
                            if opts.json {
                                progress(
                                    opts,
                                    &RunEvent::Requeued {
                                        job_id: req.id.clone(),
                                    },
                                );
                            } else {
                                eprintln!("crew: usher is back; job {} is queued again", req.id);
                            }
                            return Ok(s);
                        }
                        Err(e) => std::io::Error::other(e.to_string()),
                    }
                }
                Err(e) => e,
            };
            if started.elapsed() >= RECONNECT_BUDGET {
                return Err(ClientError::UsherLost {
                    job_id: req.id.clone(),
                    secs: RECONNECT_BUDGET.as_secs(),
                    source,
                });
            }
        }
    }

    /// Runs the command under an admission grant and returns its exit code.
    pub fn run(&self, opts: &RunOptions) -> Result<i32, ClientError> {
        self.run_with(opts, |_| Launch::default())
    }

    /// Like `run`, but `launch` sees the grant (none when the job runs unmanaged or inside a granted job) before the command starts.
    pub fn run_with(
        &self,
        opts: &RunOptions,
        launch: impl FnOnce(Option<&Grant>) -> Launch,
    ) -> Result<i32, ClientError> {
        if opts.argv.is_empty() {
            return Err(ClientError::EmptyCommand);
        }
        let mut env: BTreeMap<String, String> = std::env::vars().collect();
        let cwd = std::env::current_dir()?;
        let kind = match opts.kind {
            Some(k) => k,
            None => kind::infer_with(&opts.argv, &kind::ProjectRules::discover(&cwd)?),
        };
        let req = request::build(&Inputs {
            kind,
            argv: &opts.argv,
            cwd: &cwd,
            env: &env,
            mem_bytes: opts.mem_bytes,
            cpus: opts.cpus,
            display: opts.display,
        });

        // A command already inside a granted job runs within that job's scope and grant; asking again could deadlock on the parent's reservation.
        if opts.inside_job {
            progress(
                opts,
                &RunEvent::Unmanaged {
                    reason: UnmanagedReason::InsideGrantedJob,
                },
            );
            let argv = launch(None).apply(&opts.argv, &mut env);
            return run_nested(&argv, &cwd, env);
        }
        let mut stream = match self.connect() {
            Ok(s) => s,
            Err(e) if opts.strict => return Err(e),
            Err(e) => {
                if opts.json {
                    progress(
                        opts,
                        &RunEvent::Unmanaged {
                            reason: UnmanagedReason::UsherDown,
                        },
                    );
                } else {
                    eprintln!("crew: {e}; running unmanaged");
                }
                let argv = launch(None).apply(&opts.argv, &mut env);
                return run_unmanaged(&argv, &cwd, env);
            }
        };
        let job_id = req.id.clone();
        let queued_at = unix_ms();
        let mut lost =
            crew_proto::write_frame(&mut stream, &Envelope::of(Body::JobRequest(req.clone())))
                .is_err();

        // A queued job never falls back to an unmanaged run: if the usher goes away, wait for it and queue again.
        let grant = loop {
            if lost {
                stream = self.requeue(&req, queued_at, opts)?;
                lost = false;
            }
            let frame = match crew_proto::read_frame(&mut stream) {
                Ok(Some(frame)) => frame,
                Ok(None) | Err(FrameError::Io(_)) => {
                    lost = true;
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            let Some(Body::Reply(Reply { outcome, .. })) = frame.body else {
                return Err(ClientError::Protocol);
            };
            match outcome {
                Some(reply::Outcome::Grant(g)) => break g,
                Some(reply::Outcome::Queued(q)) => {
                    if opts.json {
                        progress(
                            opts,
                            &RunEvent::Queued {
                                position: q.position,
                                queued_ms: q.queued_ms,
                                waiting_on: q.waiting_on.into(),
                                detail: q.detail,
                            },
                        );
                    } else {
                        eprintln!("{}", describe_wait(&q));
                    }
                }
                Some(reply::Outcome::Refusal(r)) => {
                    let reason = crew_proto::RefusalReason::try_from(r.reason)
                        .unwrap_or(crew_proto::RefusalReason::Unspecified);
                    return Err(ClientError::Refused {
                        reason,
                        detail: r.detail,
                    });
                }
                None => return Err(ClientError::Protocol),
            }
        };

        progress(
            opts,
            &RunEvent::Granted {
                job_id: grant.job_id.clone(),
                scope: grant.scope_name.clone(),
            },
        );
        signals::install_termination_handler();
        let argv = launch(Some(&grant)).apply(&opts.argv, &mut env);
        let outcome = run_granted(&argv, &cwd, env, &grant);
        let (code, done) = match outcome {
            Ok(v) => v,
            Err(e) => {
                let done = JobDone {
                    job_id: job_id.clone(),
                    exit_code: -1,
                    ..JobDone::default()
                };
                let _ = crew_proto::write_frame(&mut stream, &Envelope::of(Body::JobDone(done)));
                return Err(e);
            }
        };
        progress(
            opts,
            &RunEvent::Done {
                job_id: done.job_id.clone(),
                exit_code: done.exit_code,
                wall_ms: done.wall_ms,
                cpu_usec: done.cpu_usec,
                peak_mem_bytes: done.peak_mem_bytes,
                io_read_bytes: done.io_read_bytes,
                io_write_bytes: done.io_write_bytes,
            },
        );
        let _ = crew_proto::write_frame(&mut stream, &Envelope::of(Body::JobDone(done)));
        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
        let _ = crew_proto::read_frame(&mut stream);
        Ok(code)
    }
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn progress(opts: &RunOptions, event: &RunEvent) {
    if opts.json {
        eprintln!("{}", json::event(event));
    }
}

fn run_nested(
    argv: &[String],
    cwd: &Path,
    env: BTreeMap<String, String>,
) -> Result<i32, ClientError> {
    let status = Command::new(&argv[0])
        .args(&argv[1..])
        .current_dir(cwd)
        .env_clear()
        .envs(&env)
        .status()
        .map_err(|source| ClientError::Spawn {
            command: argv[0].clone(),
            source,
        })?;
    Ok(exit_code(status))
}

fn run_unmanaged(
    argv: &[String],
    cwd: &Path,
    mut env: BTreeMap<String, String>,
) -> Result<i32, ClientError> {
    display::sanitize(&mut env, Desktop::Denied);
    let status = Command::new(&argv[0])
        .args(&argv[1..])
        .current_dir(cwd)
        .env_clear()
        .envs(&env)
        .status()
        .map_err(|source| ClientError::Spawn {
            command: argv[0].clone(),
            source,
        })?;
    Ok(exit_code(status))
}

fn run_granted(
    argv: &[String],
    cwd: &Path,
    mut env: BTreeMap<String, String>,
    grant: &Grant,
) -> Result<(i32, JobDone), ClientError> {
    for key in &grant.env_unset {
        env.remove(key);
    }
    let argv = &with_reserve(argv, grant);
    let tmpdir = grant.env_set.get("TMPDIR").map(PathBuf::from);
    let mut env_set = grant.env_set.clone();
    if tmpdir
        .as_ref()
        .is_some_and(|dir| crew_sys::private::create_dir(dir).is_err())
    {
        env_set.remove("TMPDIR");
    }
    env.extend(env_set);

    let started = Instant::now();
    let child =
        scope::spawn(argv, cwd, &env, &limits_of(grant), &grant.scope_name).map_err(|source| {
            ClientError::Spawn {
                command: argv[0].clone(),
                source,
            }
        })?;
    let (status, stats) = supervise(child, &grant.scope_name);
    if let Some(dir) = &tmpdir {
        let _ = std::fs::remove_dir_all(dir);
    }
    let code = exit_code(status?);
    let wall = started.elapsed();
    let done = JobDone {
        job_id: grant.job_id.clone(),
        exit_code: code,
        wall_ms: wall.as_millis() as u64,
        cpu_usec: stats.cpu_usec,
        peak_mem_bytes: stats.peak_worth_learning(),
        io_read_bytes: stats.io_read,
        io_write_bytes: stats.io_write,
        working_set: true,
    };
    Ok((code, done))
}

/// Wraps the command in `mbx reserve` when the usher asks for pool capacity and mbx is where it said.
pub fn with_reserve(argv: &[String], grant: &Grant) -> Vec<String> {
    grant.reserved_argv(argv)
}

/// Waits for the command, keeps the scope's last readable stats, and turns SIGINT/SIGTERM into scope kills.
fn supervise(mut child: Child, scope_name: &str) -> (std::io::Result<ExitStatus>, Stats) {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(child.wait());
    });
    let started = Instant::now();
    let mut last = Stats::default();
    let mut last_poll: Option<Instant> = None;
    let mut tick = FIRST_POLL;
    let mut signalled = false;
    loop {
        match rx.recv_timeout(tick) {
            Ok(status) => return (status, last),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return (Err(std::io::Error::other("wait thread died")), last);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        tick = (tick * 2).min(MAX_TICK);
        if signals::take_pending().is_some() {
            scope::kill(
                scope_name,
                if signalled {
                    KillSignal::Kill
                } else {
                    KillSignal::Term
                },
            );
            signalled = true;
        }
        let due = started.elapsed() < FAST_POLL_WINDOW
            || last_poll.is_none_or(|t| t.elapsed() >= STATS_EVERY);
        if due {
            last_poll = Some(Instant::now());
            if let Some(s) = scope::stats(scope_name) {
                last.observe(s);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crew_proto::PoolReserve;

    use super::*;

    fn words(cmd: &str) -> Vec<String> {
        cmd.split_whitespace().map(str::to_string).collect()
    }

    fn grant_with(r: Option<PoolReserve>) -> Grant {
        Grant {
            reserve: r,
            ..Grant::default()
        }
    }

    #[test]
    fn a_reserve_wraps_the_command_in_mbx_reserve() {
        let dir = tempfile::tempdir().unwrap();
        let mbx = dir.path().join("mbx");
        std::fs::write(&mbx, "").unwrap();
        let g = grant_with(Some(PoolReserve {
            mbx_path: mbx.display().to_string(),
            cpus: 2,
            mem_bytes: (6 << 30) + 1,
        }));
        let got = with_reserve(&words("./game --world W"), &g);
        assert_eq!(
            got[1..],
            words("reserve --cpus 2 --memory 6145MiB -- ./game --world W")
        );
        assert_eq!(got[0], mbx.display().to_string());
    }

    #[test]
    fn no_reserve_or_no_mbx_leaves_the_command_alone() {
        let argv = words("./game");
        assert_eq!(with_reserve(&argv, &grant_with(None)), argv);
        let gone = grant_with(Some(PoolReserve {
            mbx_path: "/nonexistent/mbx".into(),
            cpus: 1,
            mem_bytes: 1 << 30,
        }));
        assert_eq!(with_reserve(&argv, &gone), argv);
    }

    #[test]
    fn a_launch_changes_the_command_and_environment_only_as_asked() {
        let mut env: BTreeMap<String, String> = [("A", "1"), ("B", "2")]
            .map(|(k, v)| (k.into(), v.into()))
            .into();
        let same = Launch::default().apply(&words("cargo build"), &mut env);
        assert_eq!((same, env.len()), (words("cargo build"), 2));
        let l = Launch {
            argv: Some(words("mbx build")),
            env_set: [("C".to_string(), "3".to_string())].into(),
            env_unset: vec!["A".into()],
        };
        let argv = l.apply(&words("cargo build"), &mut env);
        assert_eq!(argv, words("mbx build"));
        assert_eq!(
            env.keys().map(String::as_str).collect::<Vec<_>>(),
            ["B", "C"]
        );
    }
}
