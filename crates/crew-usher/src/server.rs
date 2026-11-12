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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdleSource {
    Wayland,
    Off,
}

/// Open client connections, so `stop` can close them the way a dying process would.
type Open = Arc<Mutex<HashMap<u64, UnixStream>>>;

pub struct Server {
    /// Dropping it releases the state directory lock.
    _lock: fs::File,
    socket: PathBuf,
    tx: Sender<Msg>,
    actor: Option<JoinHandle<()>>,
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

fn serve_connection(stream: UnixStream, tx: &Sender<Msg>) {
    let Ok(conn) = stream.try_clone().and_then(Conn::new) else {
        return;
    };
    let mut reader = stream;
    let mut job_id: Option<String> = None;
    loop {
        let msg = match crew_proto::read_frame(&mut reader) {
            Ok(Some(env)) => match env.body {
                Some(Body::JobRequest(req)) => {
                    // A detached job belongs to the usher, so its connection closing says nothing about it.
                    if !req.detach {
                        job_id = Some(req.id.clone());
                    }
                    Msg::Job {
                        req: Box::new(req),
                        conn: conn.clone(),
                    }
                }
                Some(Body::JobDone(done)) => Msg::Done { done },
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
                if let Some(job_id) = job_id {
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

        if idle_source == IdleSource::Wayland {
            let (itx, irx) = mpsc::channel();
            match idle::spawn(Duration::from_secs(cfg.idle_timeout_secs), itx) {
                Ok(()) => {
                    log.line("IDLE watching the compositor");
                    let tx = tx.clone();
                    thread::spawn(move || {
                        while let Ok(ev) = irx.recv() {
                            if tx.send(Msg::Idle(ev)).is_err() {
                                return;
                            }
                        }
                    });
                }
                Err(e) => log.line(&format!(
                    "IDLE unavailable ({e}); use `crew owner here|away`"
                )),
            }
        }

        let daemon = Daemon::new(
            cfg,
            StatePaths {
                sizes: paths.sizes.clone(),
                jobs: paths.jobs.clone(),
                job_logs: paths.job_logs.clone(),
            },
            log.clone(),
            tx.clone(),
        );
        let actor = thread::Builder::new()
            .name("usher".into())
            .spawn(move || daemon.run(&rx))
            .expect("spawn actor");

        let accept_tx = tx.clone();
        let open: Open = Open::default();
        let accept_open = Arc::clone(&open);
        thread::Builder::new()
            .name("accept".into())
            .spawn(move || {
                let next = AtomicU64::new(0);
                for stream in listener.incoming().flatten() {
                    let tx = accept_tx.clone();
                    let id = next.fetch_add(1, Ordering::Relaxed);
                    let registry = Arc::clone(&accept_open);
                    if let Ok(copy) = stream.try_clone() {
                        registry
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .insert(id, copy);
                    }
                    thread::spawn(move || {
                        serve_connection(stream, &tx);
                        registry
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .remove(&id);
                    });
                }
            })
            .expect("spawn acceptor");

        log.line(&format!("START socket={}", paths.socket.display()));
        Ok(Self {
            _lock: lock,
            socket: paths.socket.clone(),
            tx,
            actor: Some(actor),
            open,
        })
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
