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

/// How long a root whose checkout could not be bound waits before the next
/// bind is tried, at first; each failure in a row doubles it, up to
/// [`BIND_RETRY_MAX`].
const BIND_RETRY: Duration = Duration::from_secs(5);
const BIND_RETRY_MAX: Duration = Duration::from_secs(300);

/// How long a failed build waits before it is tried again, at first; each
/// failure in a row doubles it, up to [`BUILD_RETRY_MAX`].
const BUILD_RETRY: Duration = Duration::from_secs(5);
const BUILD_RETRY_MAX: Duration = Duration::from_secs(300);

/// Roots whose last bind failed: when to try again, how many failed in a
/// row, and the error, so tool calls in between don't each reopen the store.
fn bind_failures() -> &'static std::sync::Mutex<std::collections::HashMap<PathBuf, BindFailure>> {
    static FAILURES: OnceLock<std::sync::Mutex<std::collections::HashMap<PathBuf, BindFailure>>> =
        OnceLock::new();
    FAILURES.get_or_init(Default::default)
}

struct BindFailure {
    retry_at: Instant,
    in_a_row: u32,
    error: String,
}

fn backoff(first: Duration, max: Duration, in_a_row: u32) -> Duration {
    first
        .saturating_mul(1u32 << in_a_row.saturating_sub(1).min(16))
        .min(max)
}

/// Clears a flag when dropped, so a thread that stops, by returning or by
/// a panic, never leaves its work marked as running.
struct ClearOnDrop(Arc<AtomicBool>);

impl Drop for ClearOnDrop {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// What a reader that needs exact call-graph answers gets.
pub(crate) enum Fresh {
    /// A view whose call graph covers every recorded change.
    Ready(CheckoutView),
    /// The call graph is being built or refreshed and did not finish in
    /// time.
    Building,
    /// No call graph can be served: the last build or refresh failed for
    /// this reason, and nothing is running now. A build is retried later.
    Failed(String),
}

/// When the changes since a publication are published on their own, after
/// a refresh resolves them: at once past this many changed paths, and
/// otherwise when the checkout goes quiet.
const PUBLISH_AT: usize = 64;

/// A family store holding more segments than this is built again whole,
/// which writes everything a checkout reads into one segment and retires
/// the rest; each publication of changes adds one.
const COMPACT_AT: usize = 8;

/// Whether a refresh publishes the changes it resolved.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Publish {
    /// Only past [`PUBLISH_AT`] changed paths.
    WhenMany,
    /// Whenever any path changed: the checkout went quiet.
    Always,
}

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
        {
            let failures = bind_failures()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(failure) = failures.get(root) {
                if Instant::now() < failure.retry_at {
                    return Err(DataError::Store(failure.error.clone()));
                }
            }
        }
        let family = family_of(root);
        let family_dir = storage.join("data").join(family.digest().to_string());
        let checkout = match Checkout::bind(layer(), &family_dir, family, root) {
            Ok(checkout) => {
                bind_failures()
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(root);
                Arc::new(checkout)
            }
            Err(error) => {
                let mut failures = bind_failures()
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let in_a_row = failures.get(root).map_or(0, |failure| failure.in_a_row) + 1;
                failures.insert(
                    root.to_path_buf(),
                    BindFailure {
                        retry_at: Instant::now() + backoff(BIND_RETRY, BIND_RETRY_MAX, in_a_row),
                        in_a_row,
                        error: error.to_string(),
                    },
                );
                return Err(error);
            }
        };
        let building = Arc::new(AtomicBool::new(false));
        let refreshing = Arc::new(AtomicBool::new(false));
        let quiet = {
            let checkout = Arc::downgrade(&checkout);
            let refreshing = Arc::clone(&refreshing);
            let building = Arc::clone(&building);
            Arc::new(QuietTimer::new(QUIET, move || {
                if let Some(checkout) = checkout.upgrade() {
                    start_refresh(&checkout, &refreshing, &building, Publish::Always);
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
        if handle.checkout.needs_build() {
            // Nothing published, or only the text index: a build that
            // published it first stopped before the call graph.
            start_build(&handle.checkout, &handle.building, &handle.refreshing);
        } else {
            // A bind may have recorded what changed while nothing watched.
            start_refresh(
                &handle.checkout,
                &handle.refreshing,
                &handle.building,
                Publish::WhenMany,
            );
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

    #[cfg(test)]
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
        if self.checkout.needs_build() {
            start_build(&self.checkout, &self.building, &self.refreshing);
        } else {
            start_refresh(
                &self.checkout,
                &self.refreshing,
                &self.building,
                Publish::WhenMany,
            );
        }
    }

    /// A view whose call graph covers every recorded change, for a reader
    /// that needs exact answers: refreshes now and waits for it, up to
    /// `wait`. Reports a failure at once when the last build or refresh
    /// failed and nothing is running to replace it.
    pub(crate) fn fresh_view(&self, wait: Duration) -> Fresh {
        self.request_refresh();
        let deadline = Instant::now() + wait;
        loop {
            if let Some(view) = self
                .checkout
                .view()
                .filter(|view| view.status().callgraph == super::checkout::PlaneStatus::Ready)
            {
                return Fresh::Ready(view);
            }
            let running =
                self.building.load(Ordering::Acquire) || self.refreshing.load(Ordering::Acquire);
            if !running {
                if let Some(failure) = self.checkout.failure() {
                    return Fresh::Failed(failure);
                }
            }
            if Instant::now() >= deadline {
                return Fresh::Building;
            }
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
/// by the check after it lets go of the flag. A refresh that finds the
/// stored call graph damaged starts a build, which replaces it.
fn start_refresh(
    checkout: &Arc<Checkout>,
    refreshing: &Arc<AtomicBool>,
    building: &Arc<AtomicBool>,
    publish: Publish,
) {
    let unpublished = publish == Publish::Always && checkout.changed_paths() > 0;
    if !(pending(checkout) || unpublished) || refreshing.swap(true, Ordering::AcqRel) {
        return;
    }
    let checkout = Arc::clone(checkout);
    let running = Arc::clone(refreshing);
    let building = Arc::clone(building);
    let spawned = std::thread::Builder::new()
        .name("aft-data-refresh".to_string())
        .spawn(move || {
            let mut flag = Some(ClearOnDrop(Arc::clone(&running)));
            loop {
                let result = checkout.refresh_callgraph();
                if matches!(result, Ok(Some(_))) {
                    continue;
                }
                if let Err(error) = &result {
                    crate::slog_warn!(
                        "data layer call-graph refresh failed for {}: {error}",
                        checkout.root().display()
                    );
                } else {
                    maybe_publish(&checkout, &building, &running, publish);
                }
                drop(flag.take());
                match result {
                    Err(DataError::Damaged(_)) => {
                        start_build(&checkout, &building, &running);
                        break;
                    }
                    // The changes stay pending; the next change or query that
                    // waits for the call graph tries again.
                    Err(_) => break,
                    Ok(_) => {}
                }
                if !pending(&checkout) || running.swap(true, Ordering::AcqRel) {
                    break;
                }
                flag = Some(ClearOnDrop(Arc::clone(&running)));
            }
        });
    if spawned.is_err() {
        refreshing.store(false, Ordering::Release);
    }
}

/// Publishes the changes a refresh resolved, when `publish` asks for it,
/// and builds the store again whole once publications of changes have
/// piled up.
fn maybe_publish(
    checkout: &Arc<Checkout>,
    building: &Arc<AtomicBool>,
    refreshing: &Arc<AtomicBool>,
    publish: Publish,
) {
    let changed = checkout.changed_paths();
    if changed == 0 || (publish == Publish::WhenMany && changed < PUBLISH_AT) {
        return;
    }
    match checkout.publish_changes() {
        Ok(Some(report)) => {
            crate::slog_info!(
                "data layer published changes for {}: {} paths, {} fragments, {} text units, {} segments, {:.0} ms",
                checkout.root().display(),
                report.paths,
                report.fragments,
                report.text_units,
                report.segments,
                report.ms
            );
            if report.segments > COMPACT_AT {
                start_build(checkout, building, refreshing);
            }
        }
        Ok(None) => {}
        Err(error) => crate::slog_warn!(
            "data layer could not publish changes for {}: {error}",
            checkout.root().display()
        ),
    }
}

/// Whether recorded changes wait for a published call graph. Before one is
/// published there is nothing to refresh: the build that makes it covers
/// every change recorded before its scan.
fn pending(checkout: &Checkout) -> bool {
    let status = checkout.status();
    status.callgraph_published
        && matches!(
            status.callgraph,
            super::checkout::PlaneStatus::Building { .. }
        )
}

/// Starts a build on its own thread unless one is running. A build that
/// fails is tried again after a wait that doubles each time, until one
/// succeeds or the checkout is dropped.
fn start_build(checkout: &Arc<Checkout>, building: &Arc<AtomicBool>, refreshing: &Arc<AtomicBool>) {
    if building.swap(true, Ordering::AcqRel) {
        return;
    }
    let weak = Arc::downgrade(checkout);
    let running = Arc::clone(building);
    let refreshing = Arc::clone(refreshing);
    let spawned = std::thread::Builder::new()
        .name("aft-data-build".to_string())
        .spawn(move || {
            let _flag = ClearOnDrop(Arc::clone(&running));
            let mut in_a_row = 0;
            loop {
                let Some(checkout) = weak.upgrade() else {
                    return;
                };
                match checkout.build() {
                    Ok(report) => {
                        crate::slog_info!(
                            "data layer built {}: {} files, {} parsed, {} bytes",
                            checkout.root().display(),
                            report.files,
                            report.parsed_files,
                            report.segment_bytes
                        );
                        drop(_flag);
                        // Changes recorded while the build ran stay laid over
                        // its publication and wait for the call graph.
                        start_refresh(&checkout, &refreshing, &running, Publish::WhenMany);
                        return;
                    }
                    Err(error) => {
                        in_a_row += 1;
                        let wait = backoff(BUILD_RETRY, BUILD_RETRY_MAX, in_a_row);
                        crate::slog_warn!(
                            "data layer build failed for {} (trying again in {} s): {error}",
                            checkout.root().display(),
                            wait.as_secs()
                        );
                        drop(checkout);
                        std::thread::sleep(wait);
                    }
                }
            }
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

    /// A process published a checkout's text index and stopped before its
    /// call graph. The next process to bind it builds the call graph, and
    /// its refresh thread does not spin while it waits.
    #[test]
    fn a_text_only_publication_is_built_on_the_next_bind() {
        let dir = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"demo\"\n").unwrap();
        std::fs::write(
            root.join("src/lib.rs"),
            "mod util;\npub fn caller() { util::callee(); }\n",
        )
        .unwrap();
        std::fs::write(root.join("src/util.rs"), "pub fn callee() {}\n").unwrap();
        let family = family_of(&root);
        let family_dir = storage
            .path()
            .join("data")
            .join(family.digest().to_string());
        {
            let first = Checkout::bind(layer(), &family_dir, family, &root).unwrap();
            assert!(first.publish_text_first().unwrap());
            assert!(first.needs_build());
            // Nothing to refresh before a call graph is published.
            assert!(!pending(&first));
        }
        let handle = DataCheckout::open(&root, storage.path()).unwrap();
        let view = match handle.fresh_view(Duration::from_secs(60)) {
            Fresh::Ready(view) => view,
            Fresh::Building => panic!("the call graph was never built"),
            Fresh::Failed(reason) => panic!("the build failed: {reason}"),
        };
        assert!(view.status().callgraph_published);
        assert!(!handle.checkout().needs_build());
    }

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
