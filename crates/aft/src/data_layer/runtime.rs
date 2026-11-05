//! The data layer inside a running AFT, while it is built beside the stores
//! it replaces.
//!
//! It is off unless the process runs with `AFT_DATA_LAYER=1`, an
//! undocumented switch for development: tools read from it only where they
//! have been wired to, and only when it is on. When it is on, the first
//! query of a root binds the root's checkout and, if the checkout has no
//! publication yet, builds it on a thread of its own. AFT's own edits reach
//! it through the edit-intent listener, and outside changes through the
//! watcher's path batches.
//!
//! Text queries see a change at once. The call graph is refreshed lazily,
//! since agents edit far more often than they read it (about 14 edits per
//! call-graph read in mason sessions): once when a checkout is bound or
//! built, once [`QUIET`] after the last recorded change, and whenever a
//! reader that needs exact answers waits for it ([`DataCheckout::fresh_view`]).
//! A refresh resolves every change recorded since the last one in one pass,
//! so a burst of edits costs one update.
//!
//! The builds here run on plain threads. At cutover they move onto the
//! root's executor, so an unbound root does no work and a drain cancels it.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, Instant};

use aft_store::ids::{Digest128, FamilyId};

use super::checkout::{Checkout, CheckoutView, DataError, DataLayer};
use crate::views::intent::{WriteIntent, WriteIntentListener, WritePhase};

/// Whether this process runs the data layer.
#[must_use]
pub(crate) fn enabled() -> bool {
    #[cfg(test)]
    if FORCED_ON.with(std::cell::Cell::get) {
        return true;
    }
    std::env::var_os("AFT_DATA_LAYER").is_some_and(|value| value == "1")
}

#[cfg(test)]
thread_local! {
    static FORCED_ON: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Turns the data layer on for the calling thread, so a test can run tools
/// against it without setting the process-wide switch other tests read.
#[cfg(test)]
pub(crate) fn force_on_for_test() {
    FORCED_ON.with(|forced| forced.set(true));
}

/// The process's data layer: one block cache for every checkout's reads.
fn layer() -> Arc<DataLayer> {
    static LAYER: OnceLock<Arc<DataLayer>> = OnceLock::new();
    Arc::clone(LAYER.get_or_init(|| {
        let threads = std::thread::available_parallelism().map_or(4, usize::from);
        Arc::new(DataLayer::new(256 << 20, threads))
    }))
}

/// The family a checkout at `root` belongs to: its repository's root
/// commits, so every worktree and clone of one repository shares a store.
/// A folder outside Git is a family of its own.
fn family_of(root: &Path) -> FamilyId {
    let identity = crate::search_index::canonical_git_root_commit_identity(root)
        .unwrap_or_else(|_| format!("folder:{}", root.display()));
    FamilyId::from_digest(Digest128::of(identity.as_bytes()))
}

/// How long a checkout goes without a recorded change before its call
/// graph is refreshed unasked. Edits a few seconds apart, as an agent makes
/// them, never refresh in between.
pub(crate) const QUIET: Duration = Duration::from_secs(120);

/// One root's checkout in a running AFT.
pub(crate) struct DataCheckout {
    checkout: Arc<Checkout>,
    building: Arc<AtomicBool>,
    /// A call-graph refresh is running.
    refreshing: Arc<AtomicBool>,
    quiet: Arc<QuietTimer>,
    /// Kept so the edit-intent listener lives as long as the checkout.
    _listener: Arc<dyn WriteIntentListener>,
}

impl DataCheckout {
    /// Binds the checkout at `root`, with its family store under
    /// `storage`, and builds it in the background if it has no publication.
    ///
    /// # Errors
    ///
    /// When the family store cannot be opened.
    pub(crate) fn open(root: &Path, storage: &Path) -> Result<Arc<Self>, DataError> {
        let family = family_of(root);
        let family_dir = storage.join("data").join(family.digest().to_string());
        let checkout = Arc::new(Checkout::bind(layer(), &family_dir, family, root)?);
        let building = Arc::new(AtomicBool::new(false));
        let refreshing = Arc::new(AtomicBool::new(false));
        let quiet = {
            let checkout = Arc::downgrade(&checkout);
            let refreshing = Arc::clone(&refreshing);
            Arc::new(QuietTimer::new(QUIET, move || {
                if let Some(checkout) = checkout.upgrade() {
                    start_refresh(&checkout, &refreshing);
                }
            }))
        };
        let listener: Arc<dyn WriteIntentListener> = Arc::new(EditListener {
            checkout: Arc::downgrade(&checkout),
            root: std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf()),
            quiet: Arc::clone(&quiet),
        });
        crate::views::intent::register_listener(&listener);
        let handle = Arc::new(Self {
            checkout,
            building,
            refreshing,
            quiet,
            _listener: listener,
        });
        if handle.checkout.view().is_none() {
            start_build(&handle.checkout, &handle.building, &handle.refreshing);
        } else {
            // A bind may have recorded what changed while nothing watched.
            start_refresh(&handle.checkout, &handle.refreshing);
        }
        Ok(handle)
    }

    #[must_use]
    pub(crate) fn root(&self) -> &Path {
        self.checkout.root()
    }

    #[must_use]
    pub(crate) fn view(&self) -> Option<CheckoutView> {
        self.checkout.view()
    }

    #[must_use]
    pub(crate) fn checkout(&self) -> &Checkout {
        &self.checkout
    }

    /// Records the watcher's changed `paths` (absolute).
    pub(crate) fn note_paths(&self, paths: &[PathBuf]) {
        let root = std::fs::canonicalize(self.checkout.root())
            .unwrap_or_else(|_| self.checkout.root().to_path_buf());
        record(&self.checkout, &root, paths);
        self.quiet.changed();
    }

    /// Makes sure a call-graph refresh runs if any change is pending, for a
    /// query about to wait for the call graph.
    pub(crate) fn request_refresh(&self) {
        start_refresh(&self.checkout, &self.refreshing);
    }

    /// A view whose call graph covers every recorded change, for a reader
    /// that needs exact answers: refreshes now and waits for it, up to
    /// `wait`. `None` when nothing is published yet, or the refresh did not
    /// finish in time; the reader then reports the call graph as building.
    pub(crate) fn fresh_view(&self, wait: Duration) -> Option<CheckoutView> {
        self.request_refresh();
        let deadline = Instant::now() + wait;
        loop {
            if let Some(view) = self
                .checkout
                .view()
                .filter(|view| view.status().callgraph == super::checkout::PlaneStatus::Ready)
            {
                return Some(view);
            }
            if Instant::now() >= deadline {
                return None;
            }
            // A refresh that failed leaves its changes pending and stops;
            // asking again retries it, as the next query would.
            self.request_refresh();
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// Calls `fire` once a checkout has gone `quiet` without a recorded change
/// (for the call graph, [`QUIET`], to refresh it). One timer thread runs per
/// checkout at most, and only while a change is waiting: each change moves
/// the deadline on, and the thread sleeps until the latest one.
struct QuietTimer {
    quiet: Duration,
    /// When the last change was recorded, in milliseconds since `epoch`.
    last_change_ms: AtomicU64,
    armed: AtomicBool,
    epoch: Instant,
    fire: Box<dyn Fn() + Send + Sync>,
}

impl QuietTimer {
    fn new(quiet: Duration, fire: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            quiet,
            last_change_ms: AtomicU64::new(0),
            armed: AtomicBool::new(false),
            epoch: Instant::now(),
            fire: Box::new(fire),
        }
    }

    fn now_ms(&self) -> u64 {
        u64::try_from(self.epoch.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// Notes a recorded change, and starts the timer thread unless one is
    /// waiting already.
    fn changed(self: &Arc<Self>) {
        self.last_change_ms.store(self.now_ms(), Ordering::Release);
        if self.armed.swap(true, Ordering::AcqRel) {
            return;
        }
        let timer = Arc::clone(self);
        let quiet_ms = u64::try_from(self.quiet.as_millis()).unwrap_or(u64::MAX);
        let spawned = std::thread::Builder::new()
            .name("aft-data-quiet".to_string())
            .spawn(move || loop {
                let due = timer
                    .last_change_ms
                    .load(Ordering::Acquire)
                    .saturating_add(quiet_ms);
                let now = timer.now_ms();
                if now < due {
                    std::thread::sleep(Duration::from_millis(due - now));
                    continue;
                }
                timer.armed.store(false, Ordering::Release);
                // A change recorded between the deadline check and letting
                // go of the flag found the timer armed and started none; if
                // there was one, this thread keeps waiting for it.
                if timer
                    .last_change_ms
                    .load(Ordering::Acquire)
                    .saturating_add(quiet_ms)
                    > timer.now_ms()
                    && !timer.armed.swap(true, Ordering::AcqRel)
                {
                    continue;
                }
                (timer.fire)();
                break;
            });
        if spawned.is_err() {
            self.armed.store(false, Ordering::Release);
        }
    }
}

/// Resolves the call graph again on its own thread until no recorded change
/// is pending, unless such a thread is already running. A change recorded
/// while it runs is picked up by its next pass, or, if it was just ending,
/// by the check after it lets go of the flag.
fn start_refresh(checkout: &Arc<Checkout>, refreshing: &Arc<AtomicBool>) {
    if !pending(checkout) || refreshing.swap(true, Ordering::AcqRel) {
        return;
    }
    let checkout = Arc::clone(checkout);
    let running = Arc::clone(refreshing);
    let spawned = std::thread::Builder::new()
        .name("aft-data-refresh".to_string())
        .spawn(move || loop {
            let result = checkout.refresh_callgraph();
            if matches!(result, Ok(Some(_))) {
                continue;
            }
            if let Err(error) = &result {
                // The changes stay pending; the next change or query that
                // waits for the call graph tries again.
                crate::slog_warn!(
                    "data layer call-graph refresh failed for {}: {error}",
                    checkout.root().display()
                );
            }
            running.store(false, Ordering::Release);
            if result.is_err() || !pending(&checkout) || running.swap(true, Ordering::AcqRel) {
                break;
            }
        });
    if spawned.is_err() {
        refreshing.store(false, Ordering::Release);
    }
}

/// Whether recorded changes wait for the call graph.
fn pending(checkout: &Checkout) -> bool {
    matches!(
        checkout.status().callgraph,
        super::checkout::PlaneStatus::Building { .. }
    ) && checkout.view().is_some()
}

/// Starts a build on its own thread unless one is running.
fn start_build(checkout: &Arc<Checkout>, building: &Arc<AtomicBool>, refreshing: &Arc<AtomicBool>) {
    if building.swap(true, Ordering::AcqRel) {
        return;
    }
    let checkout = Arc::clone(checkout);
    let running = Arc::clone(building);
    let refreshing = Arc::clone(refreshing);
    let spawned = std::thread::Builder::new()
        .name("aft-data-build".to_string())
        .spawn(move || {
            match checkout.build() {
                Ok(report) => crate::slog_info!(
                    "data layer built {}: {} files, {} parsed, {} bytes",
                    checkout.root().display(),
                    report.files,
                    report.parsed_files,
                    report.segment_bytes
                ),
                Err(error) => crate::slog_warn!(
                    "data layer build failed for {}: {error}",
                    checkout.root().display()
                ),
            }
            running.store(false, Ordering::Release);
            // Changes recorded while the build ran stay laid over its
            // publication and wait for the call graph.
            start_refresh(&checkout, &refreshing);
        });
    if spawned.is_err() {
        building.store(false, Ordering::Release);
    }
}

/// Records changed absolute `paths` under `root`. The checkout scans each
/// one again, a folder with everything under it, under the same rules as a
/// build, so this only turns the paths into checkout-relative form.
fn record(checkout: &Arc<Checkout>, root: &Path, paths: &[PathBuf]) {
    let mut relative_paths = Vec::with_capacity(paths.len());
    for path in paths {
        let canonical_parent = path
            .parent()
            .and_then(|parent| std::fs::canonicalize(parent).ok())
            .map(|parent| parent.join(path.file_name().unwrap_or_default()))
            .unwrap_or_else(|| path.clone());
        let Ok(relative) = canonical_parent.strip_prefix(root) else {
            continue;
        };
        let Some(relative) = relative.to_str().map(|text| text.replace('\\', "/")) else {
            continue;
        };
        if relative.is_empty() {
            continue;
        }
        relative_paths.push(relative.into_bytes());
    }
    if let Err(error) = checkout.record_paths(&relative_paths) {
        crate::slog_warn!(
            "data layer could not record changes under {}: {error}",
            checkout.root().display()
        );
    }
}

/// Hears AFT's own edits.
struct EditListener {
    checkout: Weak<Checkout>,
    root: PathBuf,
    quiet: Arc<QuietTimer>,
}

impl WriteIntentListener for EditListener {
    fn record_change(&self, change: &WriteIntent, phase: WritePhase) {
        // After the write, the bytes are final (formatting and rollback
        // included).
        if phase != WritePhase::After {
            return;
        }
        let Some(checkout) = self.checkout.upgrade() else {
            return;
        };
        match change {
            WriteIntent::Paths(paths) => record(&checkout, &self.root, paths),
            // A folder is scanned again whole, under the scan's rules.
            WriteIntent::Directory(path) => {
                record(&checkout, &self.root, std::slice::from_ref(path))
            }
        }
        self.quiet.changed();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;

    #[test]
    fn the_quiet_timer_fires_once_after_a_burst_of_changes() {
        let fired = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&fired);
        let timer = Arc::new(QuietTimer::new(Duration::from_millis(200), move || {
            count.fetch_add(1, Ordering::SeqCst);
        }));
        // Changes closer together than the quiet window never fire in
        // between.
        for _ in 0..5 {
            timer.changed();
            std::thread::sleep(Duration::from_millis(60));
        }
        assert_eq!(fired.load(Ordering::SeqCst), 0);
        let deadline = Instant::now() + Duration::from_secs(30);
        while fired.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(fired.load(Ordering::SeqCst), 1);
        // The next change arms it again.
        timer.changed();
        let deadline = Instant::now() + Duration::from_secs(30);
        while fired.load(Ordering::SeqCst) == 1 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(fired.load(Ordering::SeqCst), 2);
        std::thread::sleep(Duration::from_millis(400));
        assert_eq!(fired.load(Ordering::SeqCst), 2, "fired without a change");
    }
}
