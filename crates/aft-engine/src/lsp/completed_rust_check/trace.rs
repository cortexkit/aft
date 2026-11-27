//! Opt-in test diagnostics for a single checkout's completed Rust checks.

use std::collections::{HashMap, VecDeque};
use std::fmt::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::Instant;

use parking_lot::Mutex;

const MAX_EVENTS: usize = 128;

static TRACES: LazyLock<Mutex<HashMap<PathBuf, Arc<Timeline>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

struct Timeline {
    started: Instant,
    events: Mutex<VecDeque<String>>,
}

/// Records progress and persistence for a test checkout until the guard drops.
/// The integration test renders the timeline only if its record wait fails.
#[doc(hidden)]
pub struct CompletedRustCheckTrace {
    root: PathBuf,
    timeline: Arc<Timeline>,
}

impl CompletedRustCheckTrace {
    pub fn start(root: &Path) -> Self {
        let root = super::canonical(root).expect("completed-check trace checkout must exist");
        let timeline = Arc::new(Timeline {
            started: Instant::now(),
            events: Mutex::new(VecDeque::new()),
        });
        let mut traces = TRACES.lock();
        assert!(
            !traces.contains_key(&root),
            "duplicate completed-check trace"
        );
        traces.insert(root.clone(), Arc::clone(&timeline));
        Self { root, timeline }
    }
}

impl fmt::Display for CompletedRustCheckTrace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let events = self.timeline.events.lock();
        if events.is_empty() {
            return f.write_str("(no events recorded)");
        }
        for event in events.iter() {
            writeln!(f, "{event}")?;
        }
        Ok(())
    }
}

impl Drop for CompletedRustCheckTrace {
    fn drop(&mut self) {
        TRACES.lock().remove(&self.root);
    }
}

pub(crate) fn progress_received(
    root: &Path,
    pid: u32,
    order: u64,
    params: Option<&serde_json::Value>,
) {
    let Some(params) = params else { return };
    let Some(kind @ ("begin" | "end")) = params.pointer("/value/kind").and_then(|v| v.as_str())
    else {
        return;
    };
    record(
        root,
        format_args!(
            "reader pid={pid} order={order} progress {kind} token={:?} title={:?}",
            params.get("token"),
            params.pointer("/value/title"),
        ),
    );
}

pub(crate) fn record(root: &Path, event: fmt::Arguments<'_>) {
    let Some(timeline) = TRACES.lock().get(root).cloned() else {
        return;
    };
    let mut events = timeline.events.lock();
    let mut line = format!("+{:.3}s ", timeline.started.elapsed().as_secs_f64());
    write!(line, "{event}").expect("writing to a String cannot fail");
    if events.len() == MAX_EVENTS {
        events.pop_front();
    }
    events.push_back(line);
}
