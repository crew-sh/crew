//! Keeps the compositor idle subscription alive: restarts it with backoff and tells the actor when it is lost.
use std::io;
use std::sync::Arc;
use std::sync::mpsc::{self, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crew_sys::idle::{IdleError, IdleEvent};

use crate::daemon::{Msg, WatcherEvent};
use crate::log::Log;

#[derive(Clone, Copy, Debug)]
pub struct Backoff {
    pub first: Duration,
    pub max: Duration,
}

impl Backoff {
    pub const DEFAULT: Self = Self {
        first: Duration::from_secs(1),
        max: Duration::from_secs(60),
    };

    fn next(self, delay: Duration) -> Duration {
        (delay * 2).min(self.max)
    }
}

/// Runs `start` until the actor goes away. `start` subscribes and feeds the given sender; when its watcher ends
/// it must send `IdleEvent::Lost` there, which is what sends this loop round again.
pub fn supervise(
    mut start: impl FnMut(Sender<IdleEvent>) -> Result<(), IdleError> + Send + 'static,
    actor: Sender<Msg>,
    log: Arc<Log>,
    backoff: Backoff,
) -> io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("idle-supervisor".into())
        .spawn(move || {
            let (events, incoming) = mpsc::channel();
            let mut delay = backoff.first;
            loop {
                let started = Instant::now();
                let ended = match start(events.clone()) {
                    Ok(()) => {
                        if actor
                            .send(Msg::IdleWatcher(WatcherEvent::Watching))
                            .is_err()
                        {
                            return;
                        }
                        loop {
                            match incoming.recv() {
                                Ok(IdleEvent::Lost) => {
                                    break WatcherEvent::Lost("the subscription ended".into());
                                }
                                Ok(ev) => {
                                    if actor.send(Msg::Idle(ev)).is_err() {
                                        return;
                                    }
                                }
                                Err(_) => return,
                            }
                        }
                    }
                    Err(e) => WatcherEvent::Unavailable(e.to_string()),
                };
                log.line(&format!(
                    "IDLE {ended:?}; retrying in {}s",
                    delay.as_secs_f32()
                ));
                if actor.send(Msg::IdleWatcher(ended)).is_err() {
                    return;
                }
                // A subscription that held for a while earns a fresh, short backoff.
                if started.elapsed() > backoff.max {
                    delay = backoff.first;
                }
                thread::sleep(delay);
                delay = backoff.next(delay);
            }
        })
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    const FAST: Backoff = Backoff {
        first: Duration::from_millis(5),
        max: Duration::from_millis(40),
    };

    fn collect(rx: &mpsc::Receiver<Msg>, n: usize) -> Vec<String> {
        (0..n)
            .map(|_| match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
                Msg::IdleWatcher(WatcherEvent::Watching) => "watching".to_string(),
                Msg::IdleWatcher(WatcherEvent::Lost(_)) => "lost".to_string(),
                Msg::IdleWatcher(WatcherEvent::Unavailable(_)) => "unavailable".to_string(),
                Msg::Idle(IdleEvent::Idle) => "idle".to_string(),
                Msg::Idle(IdleEvent::Resumed) => "resumed".to_string(),
                _ => "other".to_string(),
            })
            .collect()
    }

    #[test]
    fn a_failed_subscription_is_retried_and_a_dead_one_is_replaced() {
        let attempts = Arc::new(Mutex::new(0u32));
        let counted = attempts.clone();
        let (actor, rx) = mpsc::channel();
        // Attempt 1 fails; attempt 2 works, reports idle, then dies; attempt 3 works.
        supervise(
            move |events| {
                let n = {
                    let mut a = counted.lock().unwrap();
                    *a += 1;
                    *a
                };
                match n {
                    1 => Err(IdleError::Connect("no compositor".into())),
                    2 => {
                        events.send(IdleEvent::Idle).unwrap();
                        events.send(IdleEvent::Lost).unwrap();
                        Ok(())
                    }
                    _ => Ok(()),
                }
            },
            actor,
            Arc::new(Log::sink()),
            FAST,
        )
        .unwrap();
        assert_eq!(
            collect(&rx, 5),
            ["unavailable", "watching", "idle", "lost", "watching"]
        );
        assert_eq!(*attempts.lock().unwrap(), 3);
    }

    #[test]
    fn the_delay_between_attempts_doubles_up_to_the_cap() {
        let mut d = FAST.first;
        let mut seen = Vec::new();
        for _ in 0..5 {
            seen.push(d.as_millis());
            d = FAST.next(d);
        }
        assert_eq!(seen, [5, 10, 20, 40, 40]);
    }
}
