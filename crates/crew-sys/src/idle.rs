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

fn run(
    timeout: Duration,
    tx: Sender<IdleEvent>,
    ready: &Sender<Result<(), IdleError>>,
) -> Result<(), IdleError> {
    let conn = Connection::connect_to_env().map_err(|e| IdleError::Connect(e.to_string()))?;
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
    let _notification = notifier.get_idle_notification(ms, &seat, &qh, ());
    let mut state = State { tx };
    queue
        .roundtrip(&mut state)
        .map_err(|e| IdleError::Setup(e.to_string()))?;
    let _ = ready.send(Ok(()));
    loop {
        queue
            .blocking_dispatch(&mut state)
            .map_err(|e| IdleError::Setup(e.to_string()))?;
    }
}

/// Starts a thread feeding `tx`; returns once the compositor accepted the subscription.
pub fn spawn(timeout: Duration, tx: Sender<IdleEvent>) -> Result<(), IdleError> {
    let (ready_tx, ready_rx) = mpsc::channel();
    thread::Builder::new()
        .name("idle".into())
        .spawn(move || {
            if let Err(e) = run(timeout, tx, &ready_tx) {
                let _ = ready_tx.send(Err(e));
            }
        })
        .map_err(|e| IdleError::Setup(e.to_string()))?;
    ready_rx
        .recv()
        .unwrap_or(Err(IdleError::Setup("idle thread ended".into())))
}
