use std::collections::HashMap;
use std::fs;
use std::io;
use std::net::Shutdown;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crew_proto::envelope::Body;
use crew_sys::idle;

use crate::config::Config;
use crate::daemon::{Conn, Daemon, Msg, StatePaths};
use crate::idle_watch::{self, Backoff};
use crate::log::Log;

#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("another usher already listens on {0}")]
    AlreadyRunning(String),
    #[error("socket {path}: {source}")]
    Socket { path: String, source: io::Error },
    #[error("another usher holds the state directory lock {0}; one state directory has one usher")]
    StateLocked(String),
    #[error("state directory lock {path}: {source}")]
    Lock { path: String, source: io::Error },
}

#[derive(Clone, Debug)]
pub struct Paths {
    pub socket: PathBuf,
    pub sizes: PathBuf,
    pub log: PathBuf,
    /// The state file of detached jobs.
    pub jobs: PathBuf,
    /// Default logs and exit-status files of detached jobs.
    pub job_logs: PathBuf,
    /// Held for the usher's lifetime: one usher per state directory.
    pub lock: PathBuf,
}

impl Paths {
    /// The usher's files under `state`, listening on `socket`.
    pub fn in_state(socket: PathBuf, state: &Path) -> Self {
        Self {
            socket,
            sizes: state.join("usher-sizes.pb"),
            log: state.join("usher.log"),
            jobs: state.join("usher-jobs.pb"),
            job_logs: state.join("jobs"),
            lock: state.join("usher.lock"),
        }
    }

    pub fn from_env() -> Self {
        Self::in_state(
            crew_sys::paths::socket_path(),
            &crew_sys::paths::state_dir(),
        )
    }
}

/// Starts one idle subscription that feeds the sender; tests pass their own.
pub type Subscribe =
    Arc<dyn Fn(Sender<idle::IdleEvent>) -> Result<(), idle::IdleError> + Send + Sync>;

#[derive(Clone)]
pub enum IdleSource {
    Wayland,
    Off,
    /// A stand-in compositor, retried on `backoff`.
    Custom {
        subscribe: Subscribe,
        backoff: Backoff,
    },
}

const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

/// Open client connections, so `stop` can close them the way a dying process would.
type Open = Arc<Mutex<HashMap<u64, UnixStream>>>;

pub struct Server {
    /// Dropping it releases the state directory lock.
    _lock: fs::File,
    socket: PathBuf,
    tx: Sender<Msg>,
    actor: Option<JoinHandle<()>>,
    acceptor: Option<JoinHandle<()>>,
    open: Open,
}

/// Takes the exclusive, non-blocking lock on the state directory; the kernel drops it when the usher dies.
fn lock_state(path: &Path) -> Result<fs::File, ServerError> {
    let err = |source| ServerError::Lock {
        path: path.display().to_string(),
        source,
    };
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(err)?;
    }
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(path)
        .map_err(err)?;
    // SAFETY: the descriptor is open for as long as `file` lives.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let e = io::Error::last_os_error();
        return Err(if e.kind() == io::ErrorKind::WouldBlock {
            ServerError::StateLocked(path.display().to_string())
        } else {
            err(e)
        });
    }
    Ok(file)
}

fn bind(path: &Path) -> Result<UnixListener, ServerError> {
    let err = |source| ServerError::Socket {
        path: path.display().to_string(),
        source,
    };
    if let Some(dir) = path.parent() {
        // Only a directory the usher made gets its mode set; CREW_SOCKET=$HOME/x.sock must not touch $HOME.
        let made = !dir.exists();
        fs::create_dir_all(dir).map_err(err)?;
        if made {
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(err)?;
        }
    }
    if path.exists() {
        if UnixStream::connect(path).is_ok() {
            return Err(ServerError::AlreadyRunning(path.display().to_string()));
        }
        fs::remove_file(path).map_err(err)?;
    }
    let listener = UnixListener::bind(path).map_err(err)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(err)?;
    Ok(listener)
}

/// Accepts until the process ends. Errors are logged and backed off, never spun on; connections past the cap are refused.
fn accept_loop(listener: &UnixListener, tx: &Sender<Msg>, open: &Open, log: &Log, max: usize) {
    let next = AtomicU64::new(0);
    let mut last_error = std::time::Instant::now() - Duration::from_secs(60);
    for stream in listener.incoming() {
        let mut stream = match stream {
            Ok(s) => s,
            Err(e) => {
                if last_error.elapsed() > Duration::from_secs(10) {
                    log.line(&format!("WARN accept failed: {e}"));
                    last_error = std::time::Instant::now();
                }
                thread::sleep(ACCEPT_BACKOFF);
                continue;
            }
        };
        let id = next.fetch_add(1, Ordering::Relaxed);
        let mut table = open
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if table.len() >= max {
            drop(table);
            log.line(&format!("WARN refusing a connection: {max} are open"));
            let busy = Body::Ack(crew_proto::Ack {
                ok: false,
                message: format!("the usher has {max} connections open; try again shortly"),
            });
            let _ = crew_proto::write_frame(&mut stream, &crew_proto::Envelope::of(busy));
            continue;
        }
        if let Ok(copy) = stream.try_clone() {
            table.insert(id, copy);
        }
        drop(table);
        let (tx, registry) = (tx.clone(), Arc::clone(open));
        let served = thread::Builder::new().name("client".into()).spawn(move || {
            serve_connection(stream, &tx);
            registry
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&id);
        });
        if let Err(e) = served {
            log.line(&format!("WARN cannot start a connection thread: {e}"));
            open.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&id);
            thread::sleep(ACCEPT_BACKOFF);
        }
    }
}

fn serve_connection(stream: UnixStream, tx: &Sender<Msg>) {
    let Ok(conn) = stream.try_clone().and_then(Conn::new) else {
        return;
    };
    let mut reader = stream;
    let mut job_ids: Vec<String> = Vec::new();
    loop {
        let msg = match crew_proto::read_frame(&mut reader) {
            Ok(Some(env)) => match env.body {
                Some(Body::JobRequest(req)) => {
                    // A detached job belongs to the usher, so its connection closing says nothing about it.
                    if !req.detach {
                        job_ids.push(req.id.clone());
                    }
                    Msg::Job {
                        req: Box::new(req),
                        conn: conn.clone(),
                    }
                }
                Some(Body::JobDone(done)) => Msg::Done {
                    done,
                    conn: conn.clone(),
                },
                Some(Body::TopRequest(_)) => Msg::Top { conn: conn.clone() },
                Some(Body::JobsRequest(_)) => Msg::Jobs { conn: conn.clone() },
                Some(Body::CancelRequest(c)) => Msg::Cancel {
                    job_id: c.job_id,
                    conn: conn.clone(),
                },
                Some(Body::OwnerSet(set)) => Msg::Owner {
                    set,
                    conn: conn.clone(),
                },
                Some(Body::WaitRequest(req)) => Msg::Wait {
                    req,
                    conn: conn.clone(),
                },
                _ => continue,
            },
            Ok(None) | Err(_) => {
                conn.mark_closed();
                for job_id in job_ids {
                    let _ = tx.send(Msg::Gone {
                        job_id,
                        conn: conn.clone(),
                    });
                }
                let _ = tx.send(Msg::Hangup { conn });
                return;
            }
        };
        if tx.send(msg).is_err() {
            return;
        }
    }
}

impl Server {
    /// Binds the socket and starts the admission actor; returns once clients can connect.
    pub fn start(
        cfg: Config,
        paths: &Paths,
        idle_source: IdleSource,
        echo_log: bool,
    ) -> Result<Self, ServerError> {
        let lock = lock_state(&paths.lock)?;
        let listener = bind(&paths.socket)?;
        let log = Arc::new(Log::open(&paths.log, echo_log));
        let (tx, rx) = mpsc::channel();

        let idle_timeout = Duration::from_secs(cfg.idle_timeout_secs);
        let max_connections = cfg.max_connections;
        let open: Open = Open::default();
        let mut daemon = Daemon::new(
            cfg,
            StatePaths {
                sizes: paths.sizes.clone(),
                jobs: paths.jobs.clone(),
                job_logs: paths.job_logs.clone(),
            },
            log.clone(),
            tx.clone(),
        );
        let subscribe: Option<(Subscribe, Backoff)> = match idle_source {
            IdleSource::Off => None,
            IdleSource::Wayland => Some((
                Arc::new(move |events| idle::spawn(idle_timeout, events)),
                Backoff::DEFAULT,
            )),
            IdleSource::Custom { subscribe, backoff } => Some((subscribe, backoff)),
        };
        if let Some((subscribe, backoff)) = subscribe {
            daemon = daemon.expecting_idle_watcher();
            idle_watch::supervise(
                move |events| subscribe(events),
                tx.clone(),
                log.clone(),
                backoff,
            )
            .map_err(|source| ServerError::Socket {
                path: "idle watcher thread".into(),
                source,
            })?;
        }
        let actor = thread::Builder::new()
            .name("usher".into())
            .spawn(move || daemon.run(&rx))
            .expect("spawn actor");

        let acceptor = {
            let (accept_tx, accept_open, accept_log) = (tx.clone(), Arc::clone(&open), log.clone());
            let max = max_connections;
            thread::Builder::new()
                .name("accept".into())
                .spawn(move || accept_loop(&listener, &accept_tx, &accept_open, &accept_log, max))
                .map_err(|source| ServerError::Socket {
                    path: "accept thread".into(),
                    source,
                })?
        };

        log.line(&format!("START socket={}", paths.socket.display()));
        Ok(Self {
            _lock: lock,
            socket: paths.socket.clone(),
            tx,
            actor: Some(actor),
            acceptor: Some(acceptor),
            open,
        })
    }

    /// False once the actor or the acceptor has ended; a usher without either cannot serve anyone and should exit.
    pub fn is_running(&self) -> bool {
        self.actor.as_ref().is_some_and(|h| !h.is_finished())
            && self.acceptor.as_ref().is_some_and(|h| !h.is_finished())
    }

    pub fn stop(mut self) {
        let _ = self.tx.send(Msg::Stop);
        if let Some(actor) = self.actor.take() {
            let _ = actor.join();
        }
        let _ = fs::remove_file(&self.socket);
        for stream in self
            .open
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
        {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }
}
