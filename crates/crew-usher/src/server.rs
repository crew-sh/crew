use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crew_proto::envelope::Body;
use crew_sys::idle;

use crate::config::Config;
use crate::daemon::{Conn, Daemon, Msg};
use crate::log::Log;

#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("another usher already listens on {0}")]
    AlreadyRunning(String),
    #[error("socket {path}: {source}")]
    Socket { path: String, source: io::Error },
}

#[derive(Clone, Debug)]
pub struct Paths {
    pub socket: PathBuf,
    pub sizes: PathBuf,
    pub log: PathBuf,
}

impl Paths {
    pub fn from_env() -> Self {
        let state = crew_sys::paths::state_dir();
        Self {
            socket: crew_sys::paths::socket_path(),
            sizes: state.join("usher-sizes.pb"),
            log: state.join("usher.log"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdleSource {
    Wayland,
    Off,
}

pub struct Server {
    socket: PathBuf,
    tx: Sender<Msg>,
    actor: Option<JoinHandle<()>>,
}

fn bind(path: &Path) -> Result<UnixListener, ServerError> {
    let err = |source| ServerError::Socket {
        path: path.display().to_string(),
        source,
    };
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(err)?;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(err)?;
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
                    job_id = Some(req.id.clone());
                    Msg::Job {
                        req,
                        conn: conn.clone(),
                    }
                }
                Some(Body::JobDone(done)) => Msg::Done { done },
                Some(Body::TopRequest(_)) => Msg::Top { conn: conn.clone() },
                Some(Body::OwnerSet(set)) => Msg::Owner {
                    set,
                    conn: conn.clone(),
                },
                _ => continue,
            },
            Ok(None) | Err(_) => {
                if let Some(job_id) = job_id {
                    let _ = tx.send(Msg::Gone { job_id });
                }
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

        let daemon = Daemon::new(cfg, paths.sizes.clone(), log.clone());
        let actor = thread::Builder::new()
            .name("usher".into())
            .spawn(move || daemon.run(&rx))
            .expect("spawn actor");

        let accept_tx = tx.clone();
        thread::Builder::new()
            .name("accept".into())
            .spawn(move || {
                for stream in listener.incoming().flatten() {
                    let tx = accept_tx.clone();
                    thread::spawn(move || serve_connection(stream, &tx));
                }
            })
            .expect("spawn acceptor");

        log.line(&format!("START socket={}", paths.socket.display()));
        Ok(Self {
            socket: paths.socket.clone(),
            tx,
            actor: Some(actor),
        })
    }

    pub fn stop(mut self) {
        let _ = self.tx.send(Msg::Stop);
        if let Some(actor) = self.actor.take() {
            let _ = actor.join();
        }
        let _ = fs::remove_file(&self.socket);
    }
}
