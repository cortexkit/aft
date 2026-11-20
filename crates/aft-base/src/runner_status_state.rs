//! Coalesced per-session status values; transport and publication stay with the consumer.

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, LazyLock, Weak},
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::Notify;

static CHANGED: Notify = Notify::const_new();
static SESSIONS: LazyLock<parking_lot::Mutex<Sessions>> = LazyLock::new(|| {
    let start = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    parking_lot::Mutex::new(Sessions::new(start))
});

#[derive(Clone)]
pub struct Segment {
    pub text: String,
    pub generation: u64,
    pub live: Weak<()>,
}
#[derive(Default)]
struct Session {
    routes: HashSet<usize>,
    segment: Option<Segment>,
    live: Arc<()>,
}
test_support_visibility! {
struct Sessions {
    live: HashMap<String, Session>,
    // Seed at process start to fence previous-process generations, assuming
    // no more than one segment change per millisecond of process lifetime and
    // no backwards wall-clock step across restarts. The high-water mark also
    // fences a reopened session without retaining ended sessions in memory.
    generation_floor: u64,
}
}
impl Sessions {
    test_support_visibility! {
        fn new(start_ms: u64) -> Self {
            Self {
                live: HashMap::new(),
                generation_floor: start_ms,
            }
        }
    }
    test_support_visibility! {
        fn bound(&mut self, route: usize, session: &str) {
            self.live
                .entry(session.into())
                .or_default()
                .routes
                .insert(route);
        }
    }
    test_support_visibility! {
        fn unbound(&mut self, route: usize, session: &str) {
            if let Some(live) = self.live.get_mut(session) {
                live.routes.remove(&route);
                if live.routes.is_empty() {
                    self.live.remove(session);
                }
            }
        }
    }
    test_support_visibility! {
        fn publish(&mut self, session: &str, text: &str) -> bool {
            let Some(live) = self.live.get_mut(session) else {
                return false;
            };
            if live.segment.as_ref().is_some_and(|old| old.text == text) {
                return false;
            }
            let generation = live
                .segment
                .as_ref()
                .map_or(self.generation_floor, |old| old.generation)
                + 1;
            self.generation_floor = self.generation_floor.max(generation);
            live.segment = Some(Segment {
                text: text.into(),
                generation,
                live: Arc::downgrade(&live.live),
            });
            true
        }
    }
    test_support_visibility! {
        fn snapshot(&self) -> Vec<(String, Segment)> {
            self.live
                .iter()
                .filter_map(|(session, live)| {
                    live.segment
                        .clone()
                        .map(|segment| (session.clone(), segment))
                })
                .collect()
        }
    }
}

pub fn initialize() {
    LazyLock::force(&SESSIONS);
}
pub fn route_bound(route: usize, session: &str) {
    SESSIONS.lock().bound(route, session);
}
pub fn route_unbound(route: usize, session: &str) {
    SESSIONS.lock().unbound(route, session);
    CHANGED.notify_one();
}
pub fn publish(session: &str, text: &str) {
    if SESSIONS.lock().publish(session, text) {
        CHANGED.notify_one();
    }
}

pub fn snapshot_segments() -> Vec<(String, Segment)> {
    SESSIONS.lock().snapshot()
}
pub fn notifier() -> &'static Notify {
    &CHANGED
}
#[cfg(any(test, feature = "test-support"))]
pub fn segment_for_test(session: &str) -> Option<Segment> {
    SESSIONS
        .lock()
        .live
        .get(session)
        .and_then(|live| live.segment.clone())
}
