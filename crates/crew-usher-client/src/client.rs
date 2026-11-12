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
    Envelope, Grant, JobDone, JobKind, OwnerSet, OwnerSource, Reply, TopReply, TopRequest,
    WaitingOn, reply,
};
use crew_sys::display::{self, Desktop};
use crew_sys::scope::{self, KillSignal, ScopeLimits, Stats};
use crew_sys::signals;

use crate::error::ClientError;
use crate::kind;
use crate::request::{self, Inputs};

const FIRST_POLL: Duration = Duration::from_millis(20);
const MAX_TICK: Duration = Duration::from_millis(100);
const STATS_EVERY: Duration = Duration::from_millis(500);
const FAST_POLL_WINDOW: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RunOptions {
    pub kind: Option<JobKind>,
    pub mem_bytes: Option<u64>,
    pub cpus: Option<u32>,
    pub display: bool,
    pub argv: Vec<String>,
    /// A missing daemon is an error instead of an unmanaged run.
    pub strict: bool,
}

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

    /// Runs the command under an admission grant and returns its exit code.
    pub fn run(&self, opts: &RunOptions) -> Result<i32, ClientError> {
        if opts.argv.is_empty() {
            return Err(ClientError::EmptyCommand);
        }
        let env: BTreeMap<String, String> = std::env::vars().collect();
        let cwd = std::env::current_dir()?;
        let kind = opts.kind.unwrap_or_else(|| kind::infer(&opts.argv));
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
        if env.contains_key("CREW_JOB") {
            return run_nested(&opts.argv, &cwd, env);
        }
        let mut stream = match self.connect() {
            Ok(s) => s,
            Err(e) if opts.strict => return Err(e),
            Err(e) => {
                eprintln!("crew: {e}; running unmanaged");
                return run_unmanaged(&opts.argv, &cwd, env);
            }
        };
        let job_id = req.id.clone();
        crew_proto::write_frame(&mut stream, &Envelope::of(Body::JobRequest(req)))?;

        let grant = loop {
            let Some(Body::Reply(Reply { outcome, .. })) = crew_proto::read_frame(&mut stream)?
                .ok_or(ClientError::Closed)?
                .body
            else {
                return Err(ClientError::Protocol);
            };
            match outcome {
                Some(reply::Outcome::Grant(g)) => break g,
                Some(reply::Outcome::Queued(q)) => eprintln!("{}", describe_wait(&q)),
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

        signals::install_termination_handler();
        let outcome = run_granted(&opts.argv, &cwd, env, &grant);
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
        let _ = crew_proto::write_frame(&mut stream, &Envelope::of(Body::JobDone(done)));
        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
        let _ = crew_proto::read_frame(&mut stream);
        Ok(code)
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
    let tmpdir = grant.env_set.get("TMPDIR").map(PathBuf::from);
    let mut env_set = grant.env_set.clone();
    if tmpdir
        .as_ref()
        .is_some_and(|dir| std::fs::create_dir_all(dir).is_err())
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
        peak_mem_bytes: stats.mem_peak,
        io_read_bytes: stats.io_read,
        io_write_bytes: stats.io_write,
    };
    Ok((code, done))
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
                last = Stats {
                    mem_peak: s.mem_peak.max(last.mem_peak),
                    ..s
                };
            }
        }
    }
}
