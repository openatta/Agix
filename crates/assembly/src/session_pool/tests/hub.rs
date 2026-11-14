//! `SessionHub` 把帧交给订阅者时不持着自己的锁：订阅者在 `deliver` 里回调同一个
//! hub 不会卡死，每个订阅者收到的帧仍按发出的先后。

use super::*;
use std::sync::Mutex;

fn kind(frame: &SessionFrame) -> &'static str {
    match frame.event {
        SessionEvent::EngineStopped => "stopped",
        SessionEvent::Closed => "closed",
        _ => "other",
    }
}

/// Records what it is handed; on its first frame it publishes another into
/// the same hub and asks the hub something.
struct CallsBack {
    id: u64,
    hub: std::sync::Weak<SessionHub>,
    seen: Mutex<Vec<&'static str>>,
}

impl SessionWatcher for CallsBack {
    fn id(&self) -> WatcherId {
        WatcherId(self.id)
    }

    fn deliver(&self, frame: &SessionFrame) -> bool {
        let first = {
            let mut seen = self.seen.lock().unwrap();
            seen.push(kind(frame));
            seen.len() == 1
        };
        if first {
            if let Some(hub) = self.hub.upgrade() {
                let _ = hub.ask_kind("nothing-asked");
                hub.publish(&frame_between_turns("s", SessionEvent::Closed));
            }
        }
        true
    }
}

#[test]
fn a_watcher_calling_back_into_its_hub_does_not_deadlock_and_frames_stay_in_order() {
    let hub = Arc::new(SessionHub::active_at(std::time::Instant::now()));
    let watchers: Vec<Arc<CallsBack>> = (1..=2)
        .map(|id| {
            Arc::new(CallsBack {
                id,
                hub: Arc::downgrade(&hub),
                seen: Mutex::new(Vec::new()),
            })
        })
        .collect();
    for w in &watchers {
        hub.subscribe(w.clone());
    }

    let (done, finished) = std::sync::mpsc::channel();
    let publisher = hub.clone();
    std::thread::spawn(move || {
        publisher.publish(&frame_between_turns("s", SessionEvent::EngineStopped));
        let _ = done.send(());
    });
    finished
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("publishing deadlocked on a watcher that called back into the hub");

    for w in &watchers {
        assert_eq!(
            *w.seen.lock().unwrap(),
            ["stopped", "closed", "closed"],
            "watcher {}",
            w.id
        );
    }
}

/// Takes frames unless `full`; records what it took.
struct Fills {
    id: u64,
    full: bool,
    seen: Mutex<Vec<&'static str>>,
}

impl SessionWatcher for Fills {
    fn id(&self) -> WatcherId {
        WatcherId(self.id)
    }

    fn deliver(&self, frame: &SessionFrame) -> bool {
        if self.full {
            return false;
        }
        self.seen.lock().unwrap().push(kind(frame));
        true
    }
}

/// On its first frame, subscribes `again` and publishes one more frame, as a
/// client re-subscribing while the delivery that dropped it is still going.
struct Resubscribes {
    hub: std::sync::Weak<SessionHub>,
    again: Arc<Fills>,
    done: std::sync::atomic::AtomicBool,
}

impl SessionWatcher for Resubscribes {
    fn id(&self) -> WatcherId {
        WatcherId(99)
    }

    fn deliver(&self, _: &SessionFrame) -> bool {
        if !self.done.swap(true, std::sync::atomic::Ordering::SeqCst) {
            if let Some(hub) = self.hub.upgrade() {
                hub.subscribe(self.again.clone());
                hub.publish(&frame_between_turns("s", SessionEvent::Closed));
            }
        }
        true
    }
}

#[test]
fn a_watcher_dropped_and_subscribed_again_within_one_delivery_stays_subscribed() {
    let hub = Arc::new(SessionHub::active_at(std::time::Instant::now()));
    let fills = |full| {
        Arc::new(Fills {
            id: 1,
            full,
            seen: Mutex::new(Vec::new()),
        })
    };
    let again = fills(false);
    hub.subscribe(fills(true));
    hub.subscribe(Arc::new(Resubscribes {
        hub: Arc::downgrade(&hub),
        again: again.clone(),
        done: Default::default(),
    }));

    hub.publish(&frame_between_turns("s", SessionEvent::EngineStopped));

    assert!(
        hub.is_watched_by(WatcherId(1)),
        "the new subscription was dropped"
    );
    assert_eq!(*again.seen.lock().unwrap(), ["closed"]);
}
