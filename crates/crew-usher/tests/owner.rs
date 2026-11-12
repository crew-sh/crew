//! Owner presence: what the idle watcher and `crew owner` may and may not decide. No scopes needed.
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crew_proto::{Envelope, IdleWatcher, OwnerSet, OwnerSource, envelope};
use crew_sys::idle::{IdleError, IdleEvent};
use crew_usher::config::Config;
use crew_usher::idle_watch::Backoff;
use crew_usher::server::{IdleSource, Paths, Server, Subscribe};
use crew_usher_client::{Client, OwnerArg};

/// A compositor the test controls: it can say idle or resumed, die, and come back.
#[derive(Clone, Default)]
struct FakeCompositor {
    link: Arc<Mutex<Option<Sender<IdleEvent>>>>,
    up: Arc<Mutex<bool>>,
}

impl FakeCompositor {
    fn source(&self) -> IdleSource {
        let me = self.clone();
        let subscribe: Subscribe = Arc::new(move |events| {
            if !*me.up.lock().unwrap() {
                return Err(IdleError::Connect("no compositor".into()));
            }
            *me.link.lock().unwrap() = Some(events);
            Ok(())
        });
        IdleSource::Custom {
            subscribe,
            backoff: Backoff {
                first: Duration::from_millis(20),
                max: Duration::from_millis(100),
            },
        }
    }

    fn start(&self) {
        *self.up.lock().unwrap() = true;
    }

    fn send(&self, ev: IdleEvent) {
        let link = self.link.lock().unwrap();
        link.as_ref().expect("a subscription").send(ev).unwrap();
    }

    fn die(&self) {
        *self.up.lock().unwrap() = false;
        self.send(IdleEvent::Lost);
    }
}

struct Usher {
    _dir: tempfile::TempDir,
    server: Option<Server>,
    client: Client,
}

impl Usher {
    fn start(source: IdleSource, edit: impl FnOnce(&mut Config)) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config {
            watched_paths: vec![],
            min_free_disk_gib: Default::default(),
            tmp_root: dir.path().join("jobtmp").display().to_string(),
            ..Config::default()
        };
        edit(&mut cfg);
        let paths = Paths::in_state(dir.path().join("usher.sock"), dir.path());
        let server = Server::start(cfg, &paths, source, false).unwrap();
        Self {
            client: Client::new(&paths.socket),
            _dir: dir,
            server: Some(server),
        }
    }

    fn machine(&self) -> crew_proto::MachineView {
        self.client.top().unwrap().machine.unwrap()
    }

    fn until(&self, what: &str, mut ok: impl FnMut(&crew_proto::MachineView) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !ok(&self.machine()) {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Usher {
    fn drop(&mut self) {
        if let Some(s) = self.server.take() {
            s.stop();
        }
    }
}

#[test]
fn a_watcher_that_dies_while_the_owner_is_away_makes_the_owner_present_again() {
    let compositor = FakeCompositor::default();
    compositor.start();
    let u = Usher::start(compositor.source(), |_| {});
    u.until("the watcher", |m| {
        m.idle_watcher == IdleWatcher::Watching as i32
    });
    compositor.send(IdleEvent::Idle);
    u.until("the owner to be away", |m| !m.owner_present);
    assert_eq!(u.machine().owner_source, OwnerSource::WaylandIdle as i32);

    compositor.die();
    u.until("the owner to count as present", |m| m.owner_present);
    let m = u.machine();
    assert_eq!(m.owner_source, OwnerSource::Default as i32);
    assert_ne!(m.idle_watcher, IdleWatcher::Watching as i32);

    // The compositor returns; the usher subscribes again and believes its next report.
    compositor.start();
    u.until("the watcher to return", |m| {
        m.idle_watcher == IdleWatcher::Watching as i32
    });
    assert!(u.machine().owner_present, "no report yet, so still present");
    compositor.send(IdleEvent::Idle);
    u.until("the owner to be away again", |m| !m.owner_present);
}

#[test]
fn a_usher_that_never_reached_a_compositor_says_so_and_keeps_the_owner_present() {
    let compositor = FakeCompositor::default();
    let u = Usher::start(compositor.source(), |_| {});
    let m = u.machine();
    assert!(m.owner_present);
    assert_eq!(m.idle_watcher, IdleWatcher::Unavailable as i32);
    let off = Usher::start(IdleSource::Off, |_| {});
    assert_eq!(
        off.machine().idle_watcher,
        IdleWatcher::NotConfigured as i32
    );
}

#[test]
fn an_owner_message_with_an_unknown_source_changes_nothing() {
    let u = Usher::start(IdleSource::Off, |_| {});
    let mut s = std::os::unix::net::UnixStream::connect(u.client.socket()).unwrap();
    let set = OwnerSet {
        present: false,
        source: OwnerSource::Unspecified as i32,
    };
    crew_proto::write_frame(&mut s, &Envelope::of(envelope::Body::OwnerSet(set))).unwrap();
    match crew_proto::read_frame(&mut s).unwrap().unwrap().body {
        Some(envelope::Body::Ack(a)) => assert!(!a.ok, "{}", a.message),
        other => panic!("{other:?}"),
    }
    assert!(u.machine().owner_present);
}

#[test]
fn a_manual_away_ends_after_its_maximum_age() {
    let u = Usher::start(IdleSource::Off, |c| c.manual_away_max_secs = 1);
    u.client.set_owner(OwnerArg::Away).unwrap();
    assert!(!u.machine().owner_present);
    u.until("the manual away to expire", |m| m.owner_present);
    assert_eq!(u.machine().owner_source, OwnerSource::Default as i32);
}

#[test]
fn input_after_a_manual_away_ends_it() {
    let compositor = FakeCompositor::default();
    compositor.start();
    let u = Usher::start(compositor.source(), |_| {});
    u.until("the watcher", |m| {
        m.idle_watcher == IdleWatcher::Watching as i32
    });
    u.client.set_owner(OwnerArg::Away).unwrap();
    assert!(!u.machine().owner_present);
    compositor.send(IdleEvent::Idle);
    compositor.send(IdleEvent::Resumed);
    u.until("the owner to be back", |m| m.owner_present);
    assert_eq!(u.machine().owner_source, OwnerSource::WaylandIdle as i32);
}

/// Stops the usher and starts another over the same state directory.
fn restart(u: &mut Usher, edit: impl FnOnce(&mut Config)) {
    u.server.take().unwrap().stop();
    let mut cfg = Config {
        watched_paths: vec![],
        min_free_disk_gib: Default::default(),
        tmp_root: u._dir.path().join("jobtmp").display().to_string(),
        ..Config::default()
    };
    edit(&mut cfg);
    let paths = Paths::in_state(u._dir.path().join("usher.sock"), u._dir.path());
    u.server = Some(Server::start(cfg, &paths, IdleSource::Off, false).unwrap());
}

#[test]
fn a_manual_away_survives_a_restart() {
    let mut u = Usher::start(IdleSource::Off, |_| {});
    u.client.set_owner(OwnerArg::Away).unwrap();
    restart(&mut u, |_| {});
    let m = u.machine();
    assert!(!m.owner_present);
    assert_eq!(m.owner_source, OwnerSource::Manual as i32);
}

#[test]
fn a_manual_here_survives_a_restart_and_auto_clears_it() {
    let compositor = FakeCompositor::default();
    compositor.start();
    let mut u = Usher::start(compositor.source(), |_| {});
    u.until("the watcher", |m| {
        m.idle_watcher == IdleWatcher::Watching as i32
    });
    compositor.send(IdleEvent::Idle);
    u.until("the owner to be away", |m| !m.owner_present);
    u.client.set_owner(OwnerArg::Here).unwrap();
    restart(&mut u, |_| {});
    let m = u.machine();
    assert!(m.owner_present);
    assert_eq!(m.owner_source, OwnerSource::Manual as i32);

    u.client.set_owner(OwnerArg::Auto).unwrap();
    restart(&mut u, |_| {});
    assert_eq!(u.machine().owner_source, OwnerSource::Default as i32);
}

#[test]
fn a_manual_away_keeps_its_age_across_a_restart() {
    let mut u = Usher::start(IdleSource::Off, |c| c.manual_away_max_secs = 2);
    u.client.set_owner(OwnerArg::Away).unwrap();
    thread::sleep(Duration::from_millis(2300));
    restart(&mut u, |c| c.manual_away_max_secs = 2);
    let m = u.machine();
    assert!(m.owner_present, "the away ended while the usher was down");
    assert_eq!(m.owner_source, OwnerSource::Default as i32);
}
