//! The parent session worker: finds the children, opens the model that
//! embeds queries for them, and follows each child's project-wide inspect
//! results. Each child's text, call graph and meaning plane are its own
//! checkout in the index store (see `data_layer::parent`).

use std::sync::atomic::Ordering;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use super::{
    read, write, Child, Discovery, ParentSemantic, ParentSession, Plane, DEFAULT_MAX_CHILD_REPOS,
};

/// Longest the worker sleeps between two checks of its stop flag and
/// holders when no watcher event wakes it.
const TICK: Duration = Duration::from_millis(250);

pub(super) fn run(weak: Weak<ParentSession>) {
    let Some(first) = weak.upgrade() else {
        return;
    };
    if !discover(&first) {
        return;
    }
    initial_load(&first);
    first.rounds.fetch_add(1, Ordering::SeqCst);
    crate::slog_info!(
        "parent folder loaded root={} children={}",
        first.root().display(),
        first.children().len()
    );
    let mut next_pointer_check = Instant::now() + first.pointer_interval;
    drop(first);
    loop {
        let Some(session) = weak.upgrade() else {
            return;
        };
        if session.stopped() {
            return;
        }
        if !session.held() {
            crate::slog_info!(
                "parent folder released root={} (no bound context holds it)",
                session.root().display()
            );
            session.stop();
            super::unregister(&session);
            return;
        }
        let paused = session.paused.load(Ordering::SeqCst);
        if !paused {
            let now = Instant::now();
            if now >= next_pointer_check {
                refresh_publications(&session);
                session.rounds.fetch_add(1, Ordering::SeqCst);
                next_pointer_check = Instant::now() + session.pointer_interval;
            }
        }
        let wait = if paused {
            Duration::from_millis(20)
        } else {
            TICK.min(next_pointer_check.saturating_duration_since(Instant::now()))
        };
        // Hold no strong reference while waiting, so a dropped session ends.
        drop(session);
        if let Some(session) = weak.upgrade() {
            session.wait_for_wake(wait);
        }
    }
}

/// The full discovery walk. False when the session ended meanwhile.
fn discover(session: &Arc<ParentSession>) -> bool {
    super::wait_for_discovery_hold(session.root());
    let started = Instant::now();
    let discovery = super::discover(session.root(), DEFAULT_MAX_CHILD_REPOS).unwrap_or(Discovery {
        root: session.root().to_path_buf(),
        children: Vec::new(),
        skipped: Vec::new(),
        outside: Vec::new(),
    });
    if session.stopped() {
        return false;
    }
    let children = discovery
        .children
        .iter()
        .map(|child| Arc::new(Child::new(session.root(), child.clone())))
        .collect::<Vec<_>>();
    *write(&session.children) = children;
    *write(&session.discovery) = Some(Arc::new(discovery));
    crate::slog_info!(
        "parent folder discovered root={} children={} elapsed_ms={}",
        session.root().display(),
        session.children().len(),
        started.elapsed().as_millis()
    );
    true
}

/// The first round after discovery: the query model, then each child's
/// inspect results.
fn initial_load(session: &ParentSession) {
    if session.planes.semantic {
        start_semantic(session);
    }
    for child in session.children() {
        if session.stopped() {
            return;
        }
        refresh_inspect(session, &child);
    }
}

/// Follows each child's inspect results, which its own sessions store.
fn refresh_publications(session: &ParentSession) {
    for child in session.children() {
        if session.stopped() {
            return;
        }
        refresh_inspect(session, &child);
    }
}

fn refresh_inspect(session: &ParentSession, child: &Child) {
    if read(&child.inspect).is_some() {
        return;
    }
    // Opening read-only creates nothing in the child's inspect directory but
    // a read marker; `None` means the child's sessions never stored
    // project-wide inspect results. An open error is a failure, not absence.
    match crate::inspect::cache::InspectCache::open_readonly(
        session.storage.join("inspect"),
        child.root.clone(),
    ) {
        Ok(Some(cache)) => {
            *write(&child.inspect) = Some(Arc::new(cache));
        }
        Ok(None) => {}
        Err(error) => {
            session.note_child_failure(
                child,
                "parent_inspect",
                &format!("inspect cache unreadable: {error}"),
            );
        }
    }
}

fn start_semantic(session: &ParentSession) {
    let config = &session.semantic_config;
    let started = (|| -> Result<ParentSemantic, String> {
        let mut model = crate::semantic_index::EmbeddingModel::from_config(config)?;
        let fingerprint = model.fingerprint(config)?;
        Ok(ParentSemantic {
            model: std::sync::Mutex::new(Some(model)),
            fingerprint,
        })
    })();
    *write(&session.semantic) = match started {
        Ok(semantic) => Plane::Ready(Arc::new(semantic)),
        Err(error) => {
            crate::slog_warn!(
                "parent folder embedding model unavailable root={} error={}",
                session.root.display(),
                error
            );
            Plane::Gap(format!("embedding model unavailable: {error}"))
        }
    };
}
