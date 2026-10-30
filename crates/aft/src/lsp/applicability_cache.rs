//! Process-local single-flight for complete, whole-project applicability walks.
//! Physical roots, not Git identities, are the cache boundary: linked worktrees
//! can have different untracked sources and installed TypeScript versions even
//! when their HEAD commits match.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Instant, SystemTime};

use parking_lot::{Condvar, Mutex};

use super::manager::{ApplicabilityResolutionError, ApplicabilityWalk};
use crate::config::Config;

type Key = (PathBuf, Vec<u8>);

#[derive(Clone, Copy, Debug)]
pub(crate) enum InvalidationCause {
    ContentOnlyBatch,
    CreateRemoveRename,
    SelectionInput,
    OverflowLostEvents,
    WatcherInstalled,
    WatcherRemoved,
    DependencyValidation,
}

impl InvalidationCause {
    fn label(self) -> &'static str {
        match self {
            Self::ContentOnlyBatch => "content_only_batch",
            Self::CreateRemoveRename => "create_remove_rename_or_directory",
            Self::SelectionInput => "marker_ignore_config_or_ts_install",
            Self::OverflowLostEvents => "overflow_or_lost_events",
            Self::WatcherInstalled => "watcher_installed",
            Self::WatcherRemoved => "watcher_removed",
            Self::DependencyValidation => "dependency_validation",
        }
    }
}

/// Process-lifetime counters for one physical checkout, across configuration keys.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub(crate) struct ApplicabilityMetrics {
    pub hits: u64,
    pub misses: u64,
    pub coalesced_hits: u64,
    pub invalidations: BTreeMap<&'static str, u64>,
    pub retained_content_batches: u64,
    /// Create, remove and rename batches shown not to change the cached
    /// result, which therefore kept it without a walk.
    pub retained_topology_batches: u64,
    pub warm_validations: u64,
    pub warm_validation_micros: u64,
    pub warm_validation_dependency_count: usize,
    pub dependency_count: usize,
    pub full_walks: u64,
    pub full_walk_micros: u64,
}

#[derive(PartialEq, Eq)]
enum PathState {
    Missing,
    Present {
        identity: Option<(u64, u64)>,
        modified: Option<SystemTime>,
        created: Option<SystemTime>,
        len: u64,
        is_dir: bool,
        is_file: bool,
    },
    Unreadable,
}

impl PathState {
    fn read(path: &Path) -> Self {
        match std::fs::metadata(path) {
            Ok(metadata) => Self::Present {
                identity: file_identity(path, &metadata),
                modified: metadata.modified().ok(),
                created: metadata.created().ok(),
                len: metadata.len(),
                is_dir: metadata.is_dir(),
                is_file: metadata.is_file(),
            },
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                Self::Missing
            }
            Err(_) => Self::Unreadable,
        }
    }
}

#[cfg(unix)]
fn file_identity(_path: &Path, metadata: &std::fs::Metadata) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    Some((metadata.dev(), metadata.ino()))
}

#[cfg(windows)]
fn file_identity(path: &Path, _metadata: &std::fs::Metadata) -> Option<(u64, u64)> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS,
    };
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
        .ok()?;
    let mut info = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
    // SAFETY: the open file owns a valid handle and `info` is writable. A
    // successful call initializes the structure before it is read below.
    let info = unsafe {
        if GetFileInformationByHandle(file.as_raw_handle(), info.as_mut_ptr()) == 0 {
            return None;
        }
        info.assume_init()
    };
    Some((
        u64::from(info.dwVolumeSerialNumber),
        (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
    ))
}

#[cfg(not(any(unix, windows)))]
fn file_identity(_path: &Path, _metadata: &std::fs::Metadata) -> Option<(u64, u64)> {
    None
}

struct Dependencies {
    id: u64,
    root: PathBuf,
    paths: HashMap<PathBuf, PathState>,
    inputs: HashSet<PathBuf>,
    input_identities: HashSet<(u64, u64)>,
}

thread_local! {
    // Root discovery is synchronous, including registry filters. Capture its
    // probes on this thread without changing the public root-lookup APIs.
    static DEPENDENCIES: RefCell<Option<Dependencies>> = const { RefCell::new(None) };
}

/// Record a selection input before probing it. Watcher batches check in-root
/// input membership; inputs outside the watcher still need stat rechecks.
pub(super) fn observe_path(path: &Path) {
    DEPENDENCIES.with_borrow_mut(|capture| {
        if let Some(capture) = capture {
            let path = crate::inspect::job::normalize_path(path);
            if capture.inputs.insert(path.clone()) && path.starts_with(&capture.root) {
                // Watcher paths are canonicalized. File identity also recognizes
                // a changed target reached through a marker symlink or hard link,
                // without adding a realpath call for every marker probe.
                if let PathState::Present {
                    identity: Some(identity),
                    is_file: true,
                    ..
                } = PathState::read(&path)
                {
                    capture.input_identities.insert(identity);
                    // A watcher reports a symlinked input's target, not the
                    // link. Replacing that target (rename over it) creates a
                    // new file with a new identity, so record its path too.
                    if std::fs::symlink_metadata(&path)
                        .is_ok_and(|metadata| metadata.file_type().is_symlink())
                    {
                        if let Ok(target) = std::fs::canonicalize(&path) {
                            capture
                                .inputs
                                .insert(crate::inspect::job::normalize_path(&target));
                        }
                    }
                }
            }
            if !path.starts_with(&capture.root) {
                capture
                    .paths
                    .entry(path.clone())
                    .or_insert_with(|| PathState::read(&path));
            }
        }
    });
}

pub(super) fn capture_id() -> Option<u64> {
    DEPENDENCIES.with_borrow(|capture| capture.as_ref().map(|capture| capture.id))
}

fn observe_ancestor_ignores(root: &Path) {
    // The ignore walker reads parent rules itself, outside the root-lookup
    // helpers instrumented below. Include absent rules so creating one counts.
    for dir in root.ancestors().skip(1) {
        observe_directory_ignores(dir);
    }
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        let home = PathBuf::from(home);
        observe_path(&home.join(".gitconfig"));
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"));
        observe_path(&config.join("git/config"));
    }
    if let Some(path) = ignore::gitignore::gitconfig_excludes_path() {
        observe_path(&path);
    }
}

pub(super) fn observe_directory_ignores(dir: &Path) {
    for name in [
        ".ignore",
        ".gitignore",
        ".aftignore",
        ".git",
        ".jj",
        ".git/info/exclude",
        ".git/config",
    ] {
        observe_path(&dir.join(name));
    }
}

struct Capture(Option<Dependencies>);

impl Capture {
    fn start(root: &Path) -> Self {
        static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self(DEPENDENCIES.replace(Some(Dependencies {
            id,
            // Probed paths are recorded in normalized form (no Windows `\\?\`
            // prefix), so the in-root test must compare against the same form.
            root: crate::inspect::job::normalize_path(root),
            paths: HashMap::new(),
            inputs: HashSet::new(),
            input_identities: HashSet::new(),
        })))
    }

    fn finish(
        self,
    ) -> (
        HashMap<PathBuf, PathState>,
        HashSet<PathBuf>,
        HashSet<(u64, u64)>,
    ) {
        DEPENDENCIES.with_borrow_mut(|capture| {
            let capture = capture.as_mut().expect("active capture");
            (
                std::mem::take(&mut capture.paths),
                std::mem::take(&mut capture.inputs),
                std::mem::take(&mut capture.input_identities),
            )
        })
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        DEPENDENCIES.set(self.0.take());
    }
}

fn unchanged(paths: &HashMap<PathBuf, PathState>) -> bool {
    paths.iter().all(|(path, state)| {
        matches!(
            state,
            PathState::Missing
                | PathState::Present {
                    identity: Some(_),
                    ..
                }
        ) && *state == PathState::read(path)
    })
}

#[derive(Default)]
struct State {
    generation: u64,
    running: Option<Arc<RunningScan>>,
    result: Option<ApplicabilityWalk>,
    dependencies: HashMap<PathBuf, PathState>,
    inputs: HashSet<PathBuf>,
    input_identities: HashSet<(u64, u64)>,
    marker_names: HashSet<String>,
    cause: Option<InvalidationCause>,
    /// The configuration the cached result was walked with, for resolving
    /// created files the same way the walk would.
    config: Option<Arc<Config>>,
    /// In-root directories that hold a recorded input. They include every
    /// directory the walk visited, because each one records its ignore files.
    input_dirs: Arc<HashSet<PathBuf>>,
    /// Entries that batches kept since the walk may have added: each present
    /// file they reported and each unvisited parent directory of one. Some
    /// existed already or are counted twice; counting them all against the
    /// walk's entry limit errs toward walking again.
    absorbed_entries: usize,
}

/// What a create, remove or rename batch is checked against for one cached
/// (root, configuration) result, copied out so filesystem probes run without
/// the cache locks.
struct TopologyBasis {
    flight: Arc<Flight>,
    generation: u64,
    root: PathBuf,
    walk: ApplicabilityWalk,
    config: Arc<Config>,
    input_dirs: Arc<HashSet<PathBuf>>,
    absorbed_entries: usize,
}

/// Ignore-rule files and repository markers the walker reads by itself. Those
/// in directories the walk visited are recorded inputs; these names also
/// catch ones created in directories it has not visited yet.
fn names_ignore_input(path: &Path) -> bool {
    path.file_name().is_some_and(|name| {
        [".ignore", ".gitignore", ".aftignore", ".git", ".jj"]
            .iter()
            .any(|ignore| name == *ignore)
    }) || path.ends_with(".git/info/exclude")
        || path.ends_with(".git/config")
}

/// Every in-root ancestor directory, below `root`, of the in-root inputs.
fn input_directories(root: &Path, inputs: &HashSet<PathBuf>) -> HashSet<PathBuf> {
    let mut dirs = HashSet::new();
    for input in inputs.iter().filter(|input| input.starts_with(root)) {
        for dir in input.ancestors().skip(1) {
            if dir == root || !dir.starts_with(root) || !dirs.insert(dir.to_path_buf()) {
                break;
            }
        }
    }
    dirs
}

#[derive(Default)]
struct Flight {
    state: Mutex<State>,
    /// Cache-wide use counter of the latest lookup, for capacity eviction.
    last_used: AtomicU64,
}

/// Cached (root, configuration) snapshots kept before an idle one is dropped.
pub(super) const MAX_CACHED_SNAPSHOTS: usize = 64;

#[derive(Default)]
struct Completion {
    finished: bool,
    result: Option<Result<ApplicabilityWalk, ApplicabilityResolutionError>>,
}

#[derive(Default)]
struct RunningScan {
    generation: u64,
    completion: Mutex<Completion>,
    changed: Condvar,
}

struct ScanGuard<'a> {
    flight: &'a Flight,
    scan: Arc<RunningScan>,
}

impl Drop for ScanGuard<'_> {
    fn drop(&mut self) {
        // Unwinding must release both the root's reservation and callers parked
        // on this scan. A missing result tells those callers to try a new scan.
        let mut state = self.flight.state.lock();
        if state
            .running
            .as_ref()
            .is_some_and(|scan| Arc::ptr_eq(scan, &self.scan))
        {
            state.running = None;
        }
        self.scan.completion.lock().finished = true;
        self.scan.changed.notify_all();
    }
}

#[derive(Default)]
pub(super) struct ApplicabilityCache {
    entries: Mutex<HashMap<Key, Arc<Flight>>>,
    uses: AtomicU64,
    metrics: Mutex<HashMap<PathBuf, ApplicabilityMetrics>>,
    #[cfg(test)]
    waiter_joined: (Mutex<bool>, Condvar),
}

/// Make room for one new key. Only the least recently used idle snapshots go:
/// dropping every idle one at the cap also discarded warm snapshots of roots in
/// active use, so their next lookup paid a full walk.
fn evict_least_recently_used(entries: &mut HashMap<Key, Arc<Flight>>) {
    while entries.len() >= MAX_CACHED_SNAPSHOTS {
        // Never evict a flight with a caller: doing so would admit a second
        // concurrent scan of that root and configuration.
        let Some(oldest) = entries
            .iter()
            .filter(|(_, flight)| Arc::strong_count(flight) == 1)
            .min_by_key(|(_, flight)| flight.last_used.load(Ordering::Relaxed))
            .map(|(key, _)| key.clone())
        else {
            return;
        };
        entries.remove(&oldest);
    }
}

pub(super) fn shared() -> &'static ApplicabilityCache {
    static CACHE: OnceLock<ApplicabilityCache> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

impl ApplicabilityCache {
    pub(super) fn metrics(&self, root: &Path) -> ApplicabilityMetrics {
        self.metrics.lock().get(root).cloned().unwrap_or_default()
    }

    fn record_invalidation(&self, root: &Path, cause: InvalidationCause) {
        let mut metrics = self.metrics.lock();
        *metrics
            .entry(root.into())
            .or_default()
            .invalidations
            .entry(cause.label())
            .or_default() += 1;
    }

    pub(super) fn invalidate_batch(
        &self,
        root: &Path,
        paths: &[PathBuf],
        content_only: bool,
    ) -> Option<InvalidationCause> {
        if paths.is_empty() {
            return None;
        }
        let mut known = false;
        let mut input_changed = paths
            .iter()
            .any(|path| super::typescript_project::path_affects_typescript_selection(path));
        let mut ready = true;
        let mut active_configurations = 0;
        let mut roots = HashSet::from([root.to_path_buf()]);
        // Recorded inputs are normalized; watcher paths may arrive in the
        // Windows verbatim (`\\?\C:\...`) form that `fs::canonicalize` returns.
        let normalized_paths: Vec<_> = paths
            .iter()
            .map(|path| crate::inspect::job::normalize_path(path))
            .collect();
        let file_states: Vec<_> = if content_only {
            paths.iter().map(|path| PathState::read(path)).collect()
        } else {
            Vec::new()
        };
        let mut bases = Vec::new();
        {
            let mut entries = self.entries.lock();
            // Invalidated snapshots from configurations nobody is using must
            // not keep an otherwise warm configuration from retaining edits.
            entries.retain(|(cached_root, _), flight| {
                if !(cached_root.starts_with(root) || root.starts_with(cached_root))
                    || Arc::strong_count(flight) > 1
                {
                    return true;
                }
                let state = flight.state.lock();
                state.result.is_some() || state.running.is_some()
            });
            for ((cached_root, _), flight) in entries.iter() {
                if cached_root.starts_with(root) || root.starts_with(cached_root) {
                    known = true;
                    roots.insert(cached_root.clone());
                    let state = flight.state.lock();
                    if state.result.is_some()
                        || state.running.is_some()
                        || Arc::strong_count(flight) > 1
                    {
                        active_configurations += 1;
                        ready &= state.result.is_some() && state.running.is_none();
                    }
                    input_changed |= normalized_paths.iter().any(|path| {
                        state.inputs.contains(path)
                            || state
                                .marker_names
                                .iter()
                                .any(|marker| path.ends_with(marker))
                    });
                    input_changed |= file_states.iter().any(|state_of_path| {
                        matches!(state_of_path, PathState::Present { identity: Some(identity), is_file: true, .. }
                            if state.input_identities.contains(identity))
                    });
                    if let (Some(walk), Some(config)) = (&state.result, &state.config) {
                        bases.push(TopologyBasis {
                            flight: Arc::clone(flight),
                            generation: state.generation,
                            root: crate::inspect::job::normalize_path(cached_root),
                            walk: walk.clone(),
                            config: Arc::clone(config),
                            input_dirs: Arc::clone(&state.input_dirs),
                            absorbed_entries: state.absorbed_entries,
                        });
                    }
                }
            }
        }
        // Only a positively identified data modification is safe. Missing paths,
        // directories, unreadable paths and kind-less backend events remain conservative.
        let content_only = content_only
            && file_states.iter().all(|state| {
                matches!(
                    state,
                    PathState::Present {
                        identity: Some(_),
                        is_file: true,
                        ..
                    }
                )
            });
        if content_only && known && active_configurations > 0 && ready && !input_changed {
            for root in roots {
                self.metrics
                    .lock()
                    .entry(root)
                    .or_default()
                    .retained_content_batches += 1;
            }
            return None;
        }
        if known
            && active_configurations > 0
            && ready
            && !input_changed
            && Self::absorb_topology_batch(paths, &normalized_paths, &bases)
        {
            let mut metrics = self.metrics.lock();
            for root in roots {
                metrics.entry(root).or_default().retained_topology_batches += 1;
            }
            return None;
        }
        let cause = if input_changed {
            InvalidationCause::SelectionInput
        } else if content_only {
            InvalidationCause::ContentOnlyBatch
        } else {
            InvalidationCause::CreateRemoveRename
        };
        self.invalidate(root, cause);
        Some(cause)
    }

    /// Whether a batch of creates, removes and renames leaves every cached
    /// result in `bases` as a fresh walk would find it (see
    /// [`super::manager::ApplicabilityTopologyCheck`]). The caller has ruled
    /// out recorded inputs and marker names. Files the walk reads to apply
    /// ignore rules, directories, unreadable paths, removed visited
    /// directories and removed recorded files still need a walk.
    fn absorb_topology_batch(
        paths: &[PathBuf],
        normalized_paths: &[PathBuf],
        bases: &[TopologyBasis],
    ) -> bool {
        if bases.is_empty() || normalized_paths.iter().any(|path| names_ignore_input(path)) {
            return false;
        }
        let states: Vec<_> = paths.iter().map(|path| PathState::read(path)).collect();
        let mut absorbed = Vec::with_capacity(bases.len());
        for basis in bases {
            // Classifying a created file probes markers and TypeScript installs
            // the walk may never have read. Capture them like a walk does, so a
            // later change to one invalidates the kept result.
            let capture = Capture::start(&basis.root);
            let mut check = super::manager::ApplicabilityTopologyCheck::new(
                &basis.walk,
                &basis.root,
                &basis.config,
            );
            // Entries a fresh walk would add: each present file, plus each of
            // its parent directories the walk has not visited. A watcher may
            // report a new directory's files without the directory itself.
            let mut present_files = 0;
            let mut new_dirs = HashSet::new();
            for (path, state) in normalized_paths.iter().zip(&states) {
                if !path.starts_with(&basis.root) {
                    if basis.root.starts_with(path) {
                        // The walk root or one of its ancestors changed.
                        return false;
                    }
                    // Recorded inputs outside the root were ruled out by the
                    // caller; nothing else outside it is walked.
                    continue;
                }
                match state {
                    PathState::Present { is_file: true, .. } => {
                        if !check.absorbs_present_file(path) {
                            return false;
                        }
                        present_files += 1;
                        for dir in path.ancestors().skip(1) {
                            // Visited directories form a tree up to the root.
                            if dir == basis.root
                                || basis.input_dirs.contains(dir)
                                || !new_dirs.insert(dir.to_path_buf())
                            {
                                break;
                            }
                        }
                    }
                    // A directory may hold files missing from the batch, and an
                    // unreadable path cannot be classified: walk again. A walk
                    // skips other file types (sockets, pipes), so they change
                    // nothing.
                    PathState::Present { is_dir: true, .. } | PathState::Unreadable => {
                        return false;
                    }
                    PathState::Present { .. } => {}
                    PathState::Missing => {
                        if basis.input_dirs.contains(path) || !check.survives_removed_path(path) {
                            return false;
                        }
                    }
                }
            }
            let added = present_files + new_dirs.len();
            // A walk completes with exactly the limit and fails past it.
            if basis.walk.entries() + basis.absorbed_entries + added
                > super::manager::APPLICABILITY_WALK_ENTRY_LIMIT
            {
                return false;
            }
            drop(check);
            absorbed.push((added, capture.finish()));
        }
        for (basis, (added, (dependencies, inputs, input_identities))) in bases.iter().zip(absorbed)
        {
            let new_dirs: Vec<_> = input_directories(&basis.root, &inputs)
                .into_iter()
                .filter(|dir| !basis.input_dirs.contains(dir))
                .collect();
            let mut state = basis.flight.state.lock();
            // A newer generation was invalidated or walked after these events.
            if state.generation == basis.generation {
                state.absorbed_entries += added;
                // Keep a state recorded earlier: if it differs from the one
                // read now, the next lookup's validation walks again.
                for (path, path_state) in dependencies {
                    state.dependencies.entry(path).or_insert(path_state);
                }
                if !new_dirs.is_empty() {
                    Arc::make_mut(&mut state.input_dirs).extend(new_dirs);
                }
                state.inputs.extend(inputs);
                state.input_identities.extend(input_identities);
            }
        }
        true
    }

    #[cfg(test)]
    pub(super) fn wait_for_waiter_for_test(&self, deadline: Instant) -> bool {
        let (joined, changed) = &self.waiter_joined;
        let mut joined = joined.lock();
        while !*joined {
            if changed.wait_until(&mut joined, deadline).timed_out() {
                return false;
            }
        }
        true
    }

    #[cfg(test)]
    fn note_waiter_for_test(&self) {
        *self.waiter_joined.0.lock() = true;
        self.waiter_joined.1.notify_all();
    }

    pub(super) fn walk(
        &self,
        root: &Path,
        config: &Config,
        deadline: Instant,
        mut scan: impl FnMut() -> Result<ApplicabilityWalk, ApplicabilityResolutionError>,
    ) -> Result<ApplicabilityWalk, ApplicabilityResolutionError> {
        // Configuration controls custom markers, disabled servers and ignores.
        // Key the full snapshot so no future selector can silently escape the key.
        let mut config_key = serde_json::to_value(config).expect("config serializes");
        // Only these fields are sets. Server arguments, marker priority and
        // other ordered lists must retain their configured order.
        for field in [
            "disabled_lsp",
            "lsp_auto_install_binaries",
            "lsp_inflight_installs",
        ] {
            if let Some(serde_json::Value::Array(values)) = config_key.get_mut(field) {
                values.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
            }
        }
        config_key.sort_all_objects();
        let key = (
            root.to_path_buf(),
            serde_json::to_vec(&config_key).expect("config serializes"),
        );
        let flight = {
            let mut entries = self.entries.lock();
            if !entries.contains_key(&key) {
                evict_least_recently_used(&mut entries);
            }
            let flight = Arc::clone(entries.entry(key).or_default());
            flight
                .last_used
                .store(self.uses.fetch_add(1, Ordering::Relaxed), Ordering::Relaxed);
            flight
        };
        loop {
            let mut state = flight.state.lock();
            if Instant::now() >= deadline {
                return Err(ApplicabilityResolutionError::RequestDeadline { root: root.into() });
            }
            if let Some(result) = &state.result {
                let start = Instant::now();
                let valid = unchanged(&state.dependencies);
                {
                    let mut metrics = self.metrics.lock();
                    let metrics = metrics.entry(root.into()).or_default();
                    metrics.warm_validations += 1;
                    metrics.warm_validation_micros += start.elapsed().as_micros() as u64;
                    metrics.warm_validation_dependency_count = state.dependencies.len();
                    metrics.dependency_count = state.inputs.len();
                    metrics.hits += u64::from(valid);
                }
                if valid {
                    return Ok(result.clone());
                }
                self.record_invalidation(root, InvalidationCause::DependencyValidation);
                state.result = None;
                state.cause = Some(InvalidationCause::DependencyValidation);
                state.generation = state.generation.wrapping_add(1);
            }
            if let Some(running) = &state.running {
                let running = Arc::clone(running);
                let joined_generation = running.generation == state.generation;
                drop(state);
                let mut completion = running.completion.lock();
                #[cfg(test)]
                self.note_waiter_for_test();
                while !completion.finished {
                    if running
                        .changed
                        .wait_until(&mut completion, deadline)
                        .timed_out()
                    {
                        return Err(ApplicabilityResolutionError::RequestDeadline {
                            root: root.into(),
                        });
                    }
                }
                // Retain a per-scan completion for its original waiter cohort.
                // Later binds must not consume a result invalidated before they
                // arrived, even if they waited for that scan to release the root.
                if joined_generation {
                    if let Some(result) = &completion.result {
                        self.metrics
                            .lock()
                            .entry(root.into())
                            .or_default()
                            .coalesced_hits += 1;
                        return result.clone();
                    }
                }
                continue;
            }
            let running = Arc::new(RunningScan {
                generation: state.generation,
                ..RunningScan::default()
            });
            state.running = Some(Arc::clone(&running));
            state.marker_names = super::registry::applicability_marker_names(config);
            let cause = state
                .cause
                .take()
                .map_or("cold_or_config", InvalidationCause::label);
            drop(state);
            let guard = ScanGuard {
                flight: &flight,
                scan: Arc::clone(&running),
            };
            let start = Instant::now();
            let capture = Capture::start(root);
            observe_ancestor_ignores(root);
            let root_walk_number = {
                let mut metrics = self.metrics.lock();
                let metrics = metrics.entry(root.into()).or_default();
                metrics.misses += 1;
                metrics.full_walks += 1;
                metrics.full_walks
            };
            let result = scan();
            let elapsed = start.elapsed();
            let (dependencies, inputs, input_identities) = capture.finish();
            {
                let mut metrics = self.metrics.lock();
                let metrics = metrics.entry(root.into()).or_default();
                metrics.full_walk_micros += elapsed.as_micros() as u64;
                metrics.dependency_count = inputs.len();
            }
            crate::slog_info!(
                "LSP applicability full walk root={} cause={} root_walk_number={} duration_micros={} dependencies={} success={}",
                root.display(), cause, root_walk_number, elapsed.as_micros(), inputs.len(), result.is_ok()
            );
            let input_dirs = Arc::new(input_directories(
                &crate::inspect::job::normalize_path(root),
                &inputs,
            ));
            let mut state = flight.state.lock();
            // Under continuous edits, returning one uncached scan preserves the
            // uncached walk's progress. Only unchanged scans may serve new binds.
            if state.generation == running.generation && unchanged(&dependencies) {
                if let Ok(walk) = &result {
                    state.input_dirs = input_dirs;
                    state.config = Some(Arc::new(config.clone()));
                    state.absorbed_entries = 0;
                    state.result = Some(walk.clone());
                    state.dependencies = dependencies;
                    state.inputs = inputs;
                    state.input_identities = input_identities;
                }
            }
            drop(state);
            running.completion.lock().result = Some(result.clone());
            drop(guard);
            return result;
        }
    }

    pub(super) fn invalidate(&self, root: &Path, cause: InvalidationCause) {
        let mut affected = std::collections::HashSet::new();
        affected.insert(root.to_path_buf());
        for ((cached_root, _), flight) in self.entries.lock().iter() {
            // Nested routes can observe the same change through different
            // watchers. Ancestor/descendant invalidation covers both routes.
            if cached_root.starts_with(root) || root.starts_with(cached_root) {
                let mut state = flight.state.lock();
                state.generation = state.generation.wrapping_add(1);
                state.result = None;
                state.cause = Some(cause);
                affected.insert(cached_root.clone());
            }
        }
        for root in affected {
            self.record_invalidation(&root, cause);
        }
    }
}
