use std::sync::mpsc::{self, Sender};
use std::thread;
use std::time::Duration;

use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::ext::idle_notify::v1::client::ext_idle_notification_v1::{
    self, ExtIdleNotificationV1,
};
use wayland_protocols::ext::idle_notify::v1::client::ext_idle_notifier_v1::ExtIdleNotifierV1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdleEvent {
    Idle,
    Resumed,
    /// The subscription ended (the compositor went away); no more events come from this watcher.
    Lost,
}

#[derive(Debug, thiserror::Error)]
pub enum IdleError {
    #[error("no wayland display: {0}")]
    Connect(String),
    #[error("compositor lacks {0}")]
    MissingGlobal(&'static str),
    #[error("wayland setup failed: {0}")]
    Setup(String),
}

struct State {
    tx: Sender<IdleEvent>,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_seat::WlSeat,
        _: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtIdleNotifierV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ExtIdleNotifierV1,
        _: <ExtIdleNotifierV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtIdleNotificationV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtIdleNotificationV1,
        event: ext_idle_notification_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let ev = match event {
            ext_idle_notification_v1::Event::Idled => IdleEvent::Idle,
            ext_idle_notification_v1::Event::Resumed => IdleEvent::Resumed,
            _ => return,
        };
        let _ = state.tx.send(ev);
    }
}

/// A live subscription: the connection, its event queue and the notification object that must outlive them.
struct Subscription {
    queue: wayland_client::EventQueue<State>,
    state: State,
    _notification: ExtIdleNotificationV1,
}

/// The environment's display first; when it is gone, any other `wayland-N` socket of the session (a new compositor).
fn connect() -> Result<Connection, IdleError> {
    let first = match Connection::connect_to_env() {
        Ok(conn) => return Ok(conn),
        Err(e) => e,
    };
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map(std::path::PathBuf::from);
    let mut sockets: Vec<_> = dir
        .and_then(|d| std::fs::read_dir(d).ok())
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("wayland-"))
        .map(|e| e.path())
        .filter(|p| p.extension().is_none())
        .collect();
    sockets.sort();
    sockets
        .iter()
        .find_map(|p| {
            Connection::from_socket(std::os::unix::net::UnixStream::connect(p).ok()?).ok()
        })
        .ok_or_else(|| IdleError::Connect(first.to_string()))
}

fn subscribe(timeout: Duration, tx: Sender<IdleEvent>) -> Result<Subscription, IdleError> {
    let conn = connect()?;
    let (globals, mut queue) =
        registry_queue_init::<State>(&conn).map_err(|e| IdleError::Setup(e.to_string()))?;
    let qh = queue.handle();
    let seat: wl_seat::WlSeat = globals
        .bind(&qh, 1..=1, ())
        .map_err(|_| IdleError::MissingGlobal("wl_seat"))?;
    let notifier: ExtIdleNotifierV1 = globals
        .bind(&qh, 1..=1, ())
        .map_err(|_| IdleError::MissingGlobal("ext-idle-notify-v1"))?;
    let ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
    let notification = notifier.get_idle_notification(ms, &seat, &qh, ());
    let mut state = State { tx };
    queue
        .roundtrip(&mut state)
        .map_err(|e| IdleError::Setup(e.to_string()))?;
    Ok(Subscription {
        queue,
        state,
        _notification: notification,
    })
}

/// Starts a thread feeding `tx`; returns once the compositor accepted the subscription.
/// When the thread later ends, it sends `IdleEvent::Lost` on `tx` as its last message.
pub fn spawn(timeout: Duration, tx: Sender<IdleEvent>) -> Result<(), IdleError> {
    let (ready_tx, ready_rx) = mpsc::channel();
    thread::Builder::new()
        .name("idle".into())
        .spawn(move || {
            let mut sub = match subscribe(timeout, tx.clone()) {
                Ok(sub) => {
                    let _ = ready_tx.send(Ok(()));
                    sub
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            while sub.queue.blocking_dispatch(&mut sub.state).is_ok() {}
            let _ = tx.send(IdleEvent::Lost);
        })
        .map_err(|e| IdleError::Setup(e.to_string()))?;
    ready_rx
        .recv()
        .unwrap_or(Err(IdleError::Setup("idle thread ended".into())))
}
