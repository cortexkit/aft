pub use crate::callgraph_store::access::CallgraphStoreAccess;
pub(crate) use crate::data_layer::status::EmbeddingBackendHealthSnapshot;
pub(crate) use crate::data_layer::status::{
    view_load_cause, view_load_error_response, VIEW_LOAD_DEGRADED_REASON, VIEW_LOAD_LOG_INTERVAL,
};
pub use crate::data_layer::status::{
    SemanticFillHealth, SemanticHealthComponentSnapshot, SemanticIndexEvent, SemanticIndexStatus,
    SemanticRefreshAccounting, ViewLoadFailure, SEMANTIC_MASS_REFRESH_MIN_FILES,
};
pub(crate) use crate::executor::root_lifecycle::SubcLifecycleAdmission;
pub(crate) use crate::watcher_backend::counters::{
    watcher_counters_for_root, WatcherCounters, WatcherCountersSnapshot,
};
#[cfg(test)]
pub(crate) use crate::watcher_backend::counters::{
    WatcherExclusionPlanningSnapshot, WatcherOverflowPrefix,
};

use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};
use std::io::{self, BufWriter};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex, RwLock, TryLockError, Weak};
use std::time::{Duration, Instant, SystemTime};

use crate::db::TrackedConnection;
use lsp_types::FileChangeType;
use notify::RecommendedWatcher;
use serde::Serialize;

use crate::alert_state::{
    AcceptedObservationBatch, AcceptedObservationResult, AlertDeltaState, ObservationError,
};
use crate::backup::hash_session;
use crate::backup::BackupStore;

mod read_policy_cache {
    use crate::{access_policy::AccessPolicy, db::access_grants::ScopeKey};
    use std::{cell::RefCell, rc::Rc, sync::Arc};

    // Synchronous request lifetime only. Context, scope and approval-route
    // identity keep nested calls from borrowing another authority snapshot.
    pub(super) struct Entry {
        context: usize,
        request: String,
        scope: Option<ScopeKey>,
        scope_grants_denial: Option<crate::access_policy::ScopeGrantsDenial>,
        route_generation: u64,
        policy: RefCell<Option<Arc<AccessPolicy>>>,
        unrestored: RefCell<Option<Vec<crate::context::launch_grants::UnrestoredGrant>>>,
    }

    impl Entry {
        fn clear(&self) {
            self.policy.borrow_mut().take();
            self.unrestored.borrow_mut().take();
        }

        pub(super) fn get(&self) -> Option<Arc<AccessPolicy>> {
            self.policy.borrow().clone()
        }
        pub(super) fn set(&self, policy: Arc<AccessPolicy>) {
            *self.policy.borrow_mut() = Some(policy);
        }
        pub(super) fn unrestored(
            &self,
        ) -> Option<Vec<crate::context::launch_grants::UnrestoredGrant>> {
            self.unrestored.borrow().clone()
        }
        pub(super) fn set_unrestored(
            &self,
            grants: Vec<crate::context::launch_grants::UnrestoredGrant>,
        ) {
            *self.unrestored.borrow_mut() = Some(grants);
        }
    }

    thread_local! {
        static CURRENT: RefCell<Option<Rc<Entry>>> = const { RefCell::new(None) };
    }

    pub(super) fn current(context: usize, request: &str) -> Option<Rc<Entry>> {
        CURRENT.with(|slot| {
            slot.borrow()
                .as_ref()
                .filter(|entry| {
                    let authority_matches = crate::access_policy::scope_matches(&entry.scope)
                        && entry.scope_grants_denial == crate::access_policy::scope_grants_denial()
                        && entry.route_generation == crate::access_policy::ask_route_generation();
                    if !authority_matches {
                        entry.clear();
                    }
                    entry.context == context && entry.request == request && authority_matches
                })
                .cloned()
        })
    }

    pub(super) fn with<R>(context: usize, request: &str, run: impl FnOnce() -> R) -> R {
        struct Restore(Option<Rc<Entry>>);
        impl Drop for Restore {
            fn drop(&mut self) {
                CURRENT.with(|slot| {
                    slot.replace(self.0.take());
                });
            }
        }
        let entry = Rc::new(Entry {
            context,
            request: request.to_owned(),
            scope: crate::access_policy::current_scope(),
            scope_grants_denial: crate::access_policy::scope_grants_denial(),
            route_generation: crate::access_policy::ask_route_generation(),
            policy: RefCell::new(None),
            unrestored: RefCell::new(None),
        });
        let previous = CURRENT.with(|slot| slot.replace(Some(entry)));
        let _restore = Restore(previous);
        if let Some(previous) = &_restore.0 {
            // A nested call under another authority must not revive the outer
            // call's old grants when its cache slot is restored on return.
            if !crate::access_policy::scope_matches(&previous.scope)
                || previous.scope_grants_denial != crate::access_policy::scope_grants_denial()
                || previous.route_generation != crate::access_policy::ask_route_generation()
            {
                previous.clear();
            }
        }
        run()
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn read_policy_cache_restores_outer_entry_and_clears_on_unwind() {
            assert!(CURRENT.with(|slot| slot.borrow().is_none()));
            with(7, "same-request", || {
                let outer = current(7, "same-request").unwrap();
                with(7, "same-request", || {
                    let inner = current(7, "same-request").unwrap();
                    assert!(!Rc::ptr_eq(&outer, &inner));
                });
                assert!(Rc::ptr_eq(&outer, &current(7, "same-request").unwrap()));
            });
            assert!(CURRENT.with(|slot| slot.borrow().is_none()));
            let panicked = std::panic::catch_unwind(|| {
                with(7, "same-request", || panic!("cache lifetime probe"));
            });
            assert!(panicked.is_err());
            assert!(CURRENT.with(|slot| slot.borrow().is_none()));
            with(7, "same-request", || {
                assert!(current(7, "same-request").is_some())
            });
            assert!(CURRENT.with(|slot| slot.borrow().is_none()));
        }
    }
}

#[cfg(test)]
pub(crate) mod policy_load_probe {
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex, OnceLock},
    };
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub(crate) struct Counts {
        pub policies: usize,
        pub grants: usize,
        pub read_paths: usize,
    }
    type Shared = Arc<Mutex<Counts>>;
    fn probes() -> &'static Mutex<HashMap<String, Shared>> {
        static PROBES: OnceLock<Mutex<HashMap<String, Shared>>> = OnceLock::new();
        PROBES.get_or_init(Mutex::default)
    }
    pub(crate) struct Probe {
        request: String,
        counts: Shared,
    }
    impl Probe {
        pub(crate) fn new(request: &str) -> Self {
            let counts = Arc::new(Mutex::new(Counts::default()));
            probes()
                .lock()
                .unwrap()
                .insert(request.to_owned(), counts.clone());
            Self {
                request: request.to_owned(),
                counts,
            }
        }
        pub(crate) fn counts(&self) -> Counts {
            *self.counts.lock().unwrap()
        }
    }
    impl Drop for Probe {
        fn drop(&mut self) {
            probes().lock().unwrap().remove(&self.request);
        }
    }
    pub(super) fn policy(request: &str) {
        if let Some(counts) = probes().lock().unwrap().get(request) {
            counts.lock().unwrap().policies += 1;
        }
    }
    pub(super) fn grants(request: &str) {
        if let Some(counts) = probes().lock().unwrap().get(request) {
            counts.lock().unwrap().grants += 1;
        }
    }
    pub(super) fn read_path(request: &str) {
        if let Some(counts) = probes().lock().unwrap().get(request) {
            counts.lock().unwrap().read_paths += 1;
        }
    }
}
use crate::bash_background::{BgCompletion, BgTaskHealthCounts, BgTaskRegistry};
use crate::checkpoint::CheckpointStore;
use crate::config::Config;
use crate::harness::Harness;
use crate::inspect::{
    InspectCategory, InspectManager, InspectSnapshot, Tier2ExternalGates, Tier2RefreshScheduler,
    Tier2TriggerReason,
};
use crate::language::LanguageProvider;
use crate::lsp::manager::{LspManager, StaleDiagnosticsMark};
use crate::lsp::registry::is_config_file_path_with_custom;
use crate::parser::{SharedSymbolCache, SymbolCache, TreeSitterProvider};
use crate::protocol::{
    ConfigureWarningsFrame, ProgressFrame, PushFrame, StatusChangedFrame, StatusPayload,
};
use crate::watcher_filter::WatcherJoinOutcome;
use crate::watcher_filter::{SharedGitignore, WatcherDispatchEvent, WatcherThreadHandle};

use crate::data_layer::ignore_rules as gitignore_state;
#[path = "context_index_host.rs"]
mod index_host;
pub(crate) use gitignore_state::IgnoreRuleChange;

// The shared ignore-rule definition every project walker applies; see
// `apply_project_ignore_rules`.
pub(crate) use gitignore_state::apply_project_ignore_rules;

#[path = "confinement.rs"]
pub(crate) mod confinement;
#[path = "launch_grants.rs"]
pub(crate) mod launch_grants;
#[cfg(test)]
pub(crate) use gitignore_state::ignore_rules_fixture;

pub type ProgressSender = Arc<Box<dyn Fn(PushFrame) + Send + Sync>>;
pub type SharedProgressSender = Arc<Mutex<Option<ProgressSender>>>;

thread_local! {
    static REQUEST_PROGRESS_ORIGIN: std::cell::RefCell<Option<(String, crate::protocol::ProgressOrigin)>> = const { std::cell::RefCell::new(None) };
}

pub(crate) struct ProgressOriginGuard(Option<(String, crate::protocol::ProgressOrigin)>);

impl Drop for ProgressOriginGuard {
    fn drop(&mut self) {
        REQUEST_PROGRESS_ORIGIN.with(|origin| *origin.borrow_mut() = self.0.take());
    }
}

/// Scope progress to the exact route generation that admitted a request. Handles
/// captured inside the scope retain that destination when used on other threads.
pub(crate) fn progress_origin_guard(
    request_id: &str,
    channel: u16,
    epoch: u32,
    corr: u64,
) -> ProgressOriginGuard {
    ProgressOriginGuard(REQUEST_PROGRESS_ORIGIN.with(|origin| {
        origin.borrow_mut().replace((
            request_id.to_owned(),
            crate::protocol::ProgressOrigin {
                channel,
                epoch,
                corr,
            },
        ))
    }))
}
pub type SharedStdoutWriter = Arc<Mutex<BufWriter<io::Stdout>>>;
const STATUS_DEBOUNCE_MS: u64 = 1_000;

thread_local! {
    /// Config snapshots pinned by the requests running on this thread, keyed
    /// by the address of the `AppContext` they belong to.
    static CONFIG_PINS: std::cell::RefCell<Vec<(usize, Arc<Config>)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

thread_local! {}

#[cfg(test)]
thread_local! {
    static SEMANTIC_FAMILY_ARENA_MEASUREMENTS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

thread_local! {
    /// Set while this thread runs a closure under the configuration write
    /// lock (`update_config`). Reading the configuration there would wait on
    /// that same lock forever, so the read path panics instead.
    static CONFIG_WRITE_LOCK_HELD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Marks the configuration write lock as held by this thread until dropped,
/// including when the closure panics.
struct ConfigWriteLockHeld {
    previous: bool,
}

impl ConfigWriteLockHeld {
    fn enter() -> Self {
        Self {
            previous: CONFIG_WRITE_LOCK_HELD.with(|held| held.replace(true)),
        }
    }
}

impl Drop for ConfigWriteLockHeld {
    fn drop(&mut self) {
        CONFIG_WRITE_LOCK_HELD.with(|held| held.set(self.previous));
    }
}

fn assert_config_write_lock_not_held() {
    if CONFIG_WRITE_LOCK_HELD.with(std::cell::Cell::get) {
        panic!(
            "config read inside update_config closure would deadlock: the closure \
             receives the configuration as `&mut Config` and must not call \
             `ctx.config()`, publish the configuration, or call anything that does"
        );
    }
}

fn pinned_config_for(key: usize) -> Option<Arc<Config>> {
    CONFIG_PINS.with(|pins| {
        pins.borrow()
            .iter()
            .find(|(pinned_key, _)| *pinned_key == key)
            .map(|(_, config)| Arc::clone(config))
    })
}

fn replace_pinned_config(key: usize, config: &Arc<Config>) {
    CONFIG_PINS.with(|pins| {
        if let Some(slot) = pins
            .borrow_mut()
            .iter_mut()
            .find(|(pinned_key, _)| *pinned_key == key)
        {
            slot.1 = Arc::clone(config);
        }
    });
}

// `try_update` is only available from Rust 1.99; retain checked allocation on the 1.92 MSRV.
#[allow(deprecated)]
fn next_config_pin_identity() -> usize {
    static NEXT: AtomicUsize = AtomicUsize::new(1);
    // A pin can survive moving or dropping its context. Addresses are reusable,
    // whereas these identities never name a later context on the same thread.
    NEXT.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
        .expect("context identity exhausted")
}

/// Releases a request's config pin; see [`AppContext::pin_config`].
pub struct ConfigPinGuard {
    key: Option<usize>,
    // The pin lives in a thread-local, so the guard must drop on this thread.
    _not_send: std::marker::PhantomData<*const ()>,
}

impl Drop for ConfigPinGuard {
    fn drop(&mut self) {
        if let Some(key) = self.key {
            CONFIG_PINS.with(|pins| {
                pins.borrow_mut()
                    .retain(|(pinned_key, _)| *pinned_key != key)
            });
        }
    }
}

#[cfg(test)]
thread_local! {
    /// Ignore-file walks run by this thread; see
    /// `AppContext::ignore_walks_on_current_thread_for_test`.
    static IGNORE_WALKS_ON_THREAD: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Canonicalize a path that may no longer exist (pending callgraph paths
/// legitimately include deleted files): canonicalize the nearest existing
/// ancestor of the ORIGINAL spelling and re-append the missing tail, so alias
/// spellings (macOS /var vs /private/var) normalize even for dead paths.
///
/// Symlink semantics match the callgraph store's `normalize_file_path`
/// (filesystem-first): `root/link/../x` where `link` targets a foreign
/// directory canonicalizes to the FOREIGN parent, not a lexical `root/x`.
/// Lexical `.`/`..` resolution applies only past the deepest existing
/// component (a nonexistent component cannot be a symlink) and to the tail
/// appended onto an already-canonical, symlink-free base.
/// Component-wise lenient canonicalization with filesystem-first semantics
/// (matching the callgraph store's `normalize_file_path`): each existing
/// component — including symlinks — resolves through the filesystem; genuinely
/// absent components accumulate on a missing stack and resolve lexically. `..`
/// pops the missing stack first, and only when the stack is empty does it take
/// the parent of the canonical base (symlink-free, so a lexical parent is
/// sound there). Handles re-entry: in `dead/../link/../x`, `dead/..` drains
/// back to the existing base and `link` (a symlink) resolves through the
/// filesystem instead of being erased lexically.
///
/// Returns `None` — and containment fails closed — where realpath would not
/// resolve either: a dangling symlink or other filesystem error on an existing
/// component (the store falls back to the raw spelling for those, which
/// `relative_path` keeps as an absolute out-of-root key), and `..` traversal
/// through a non-directory (realpath ENOTDIR).
pub(crate) fn canonicalize_lenient(path: &Path) -> Option<PathBuf> {
    use std::path::Component;
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return Some(canonical);
    }
    let mut resolved = PathBuf::new();
    let mut missing: Vec<std::ffi::OsString> = Vec::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => {
                resolved.push(component.as_os_str());
                // Canonicalize the anchor so a missing child directly under a
                // drive/UNC root compares in the same (verbatim) spelling as a
                // canonicalized root on Windows; "/" is a no-op on Unix.
                if let Ok(canonical_anchor) = std::fs::canonicalize(&resolved) {
                    resolved = canonical_anchor;
                }
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if missing.pop().is_none() {
                    if !resolved.as_os_str().is_empty() && !resolved.is_dir() {
                        // `file/..` — realpath rejects with ENOTDIR.
                        return None;
                    }
                    resolved.pop();
                }
            }
            Component::Normal(name) => {
                if missing.is_empty() {
                    let candidate = resolved.join(name);
                    match std::fs::canonicalize(&candidate) {
                        Ok(canonical) => resolved = canonical,
                        Err(_) => match std::fs::symlink_metadata(&candidate) {
                            // Genuinely absent: lexical from here until `..`
                            // drains back.
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                                missing.push(name.to_owned())
                            }
                            // Exists but does not canonicalize (dangling
                            // symlink) or the probe itself failed: fail closed.
                            _ => return None,
                        },
                    }
                } else {
                    missing.push(name.to_owned());
                }
            }
        }
    }
    for name in missing {
        resolved.push(name);
    }
    Some(resolved)
}

/// Numeric projection for consumers that still require the legacy status-bar
/// shape. It is derived from [`StatusBarCountValues`], which preserves whether
/// each category is present instead of converting missing values to zero.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StatusBarCounts {
    pub errors: usize,
    pub warnings: usize,
    pub todos: usize,
}

/// Proven status values. A missing category has not produced a trustworthy
/// value and remains absent instead of being converted to a clean zero.
///
/// The bar carries only what stays cheap and current: LSP errors and
/// warnings, and the TODO count. The whole-repository analyses (dead code,
/// unused exports, duplicates, cycles, complexity) run only when an agent
/// calls `aft_inspect`, so the bar has no counts for them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StatusBarCountValues {
    pub disabled_categories: Vec<&'static str>,
    pub errors: Option<usize>,
    pub warnings: Option<usize>,
    pub todos: Option<usize>,
    pub diagnostics: Option<&'static str>,
    /// Why `diagnostics` is `"pending"`: `applicability_walk_running` while
    /// the background walk that decides which language servers apply to the
    /// project has not finished, `applicability_unknown` when no such walk has
    /// recorded a result (it failed or never ran), and
    /// `producer_reports_pending` when an applicable server has not started,
    /// is still warming up, or has not reported yet. `None` otherwise.
    pub diagnostics_reason: Option<&'static str>,
    pub diagnostics_gaps: Vec<serde_json::Value>,
}

impl StatusBarCountValues {
    fn mask_disabled(mut self, inspect: &crate::config::InspectConfig) -> Self {
        self.disabled_categories = InspectCategory::active()
            .iter()
            .copied()
            .filter(|category| {
                *category != InspectCategory::Metrics && !inspect.category_enabled(*category)
            })
            .map(InspectCategory::as_str)
            .collect();
        if self.disabled_categories.contains(&"diagnostics") {
            self.errors = None;
            self.warnings = None;
        }
        if self.disabled_categories.contains(&"todos") {
            self.todos = None;
        }
        self
    }
    pub(crate) fn legacy_projection(&self) -> Option<StatusBarCounts> {
        let [Some(errors), Some(warnings), Some(todos)] = [self.errors, self.warnings, self.todos]
        else {
            return None;
        };

        Some(StatusBarCounts {
            errors,
            warnings,
            todos,
        })
    }
}

/// Last-known TODO count, refreshed off the hot path. `errors` and `warnings`
/// are intentionally NOT cached here — they're read live per attach. `None`
/// means no scan has produced a count yet, so we never fabricate a `0`.
#[derive(Debug, Clone, Default)]
struct StatusBarTodos {
    todos: Option<usize>,
    generation: u64,
}

#[derive(Debug, Clone, Default)]
struct StatusBarCache {
    valid: bool,
    diagnostics_authoritative: bool,
    diagnostics_generation: u64,
    todos_generation: u64,
    tsconfig_generation: u64,
    counts: Option<StatusBarCountValues>,
}

/// Deduplicates the agent-facing status bar so it appears in tool-result text
/// only when a value changes. It compares the omission-preserving values, so a
/// category arriving (`?` becoming a number) counts as a change.
#[derive(Debug, Default)]
struct StatusBarEmission(RwLock<std::collections::VecDeque<(String, StatusBarCountValues)>>);

impl StatusBarEmission {
    fn should_emit(&self, session_id: &str, counts: &StatusBarCountValues) -> bool {
        let mut last = self
            .0
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(index) = last.iter().position(|(session, _)| session == session_id) {
            let (_, previous) = last.remove(index).expect("located session");
            let changed = previous != *counts;
            last.push_back((session_id.to_string(), counts.clone()));
            return changed;
        }
        // Eviction only repeats a bar for an idle session; it never hides a change.
        if last.len() == 256 {
            last.pop_front();
        }
        last.push_back((session_id.to_string(), counts.clone()));
        true
    }

    fn clear(&self) {
        self.0
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RootHealthState {
    Ready,
    Busy,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct HealthComponentSnapshot {
    pub status: &'static str,
}

/// Live counters for an in-progress semantic embedding build. The worker updates
/// only atomics at batch boundaries, so progress reporting never contends with
/// embedding requests.
#[derive(Debug, Clone, Default)]
pub struct SemanticBuildProgress {
    embedded_chunks: Arc<AtomicUsize>,
    total_chunks: Arc<AtomicUsize>,
    current_batch: Arc<AtomicUsize>,
    total_batches: Arc<AtomicUsize>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SemanticBuildProgressSnapshot {
    pub embedded_chunks: usize,
    pub total_chunks: usize,
    pub current_batch: usize,
    pub total_batches: usize,
}

impl SemanticBuildProgress {
    pub fn report(&self, embedded_chunks: usize, total_chunks: usize, batch_size: usize) {
        let batch_size = batch_size.max(1);
        let total_batches = total_chunks.div_ceil(batch_size);
        self.total_chunks.store(total_chunks, Ordering::Relaxed);
        self.embedded_chunks
            .store(embedded_chunks.min(total_chunks), Ordering::Relaxed);
        self.current_batch.store(
            embedded_chunks.min(total_chunks).div_ceil(batch_size),
            Ordering::Relaxed,
        );
        self.total_batches.store(total_batches, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> SemanticBuildProgressSnapshot {
        SemanticBuildProgressSnapshot {
            embedded_chunks: self.embedded_chunks.load(Ordering::Relaxed),
            total_chunks: self.total_chunks.load(Ordering::Relaxed),
            current_batch: self.current_batch.load(Ordering::Relaxed),
            total_batches: self.total_batches.load(Ordering::Relaxed),
        }
    }
}

/// Per-project checkout view load health. Counts every load attempt the
/// queries actually made (grep, glob, aft_search binds for the lexical view;
/// navigation, zoom and inspect for the callgraph view), so a persistent
/// outage keeps counting while it keeps failing queries. Measured reserve
/// refusals are active storage conditions, separate from failure history.
#[derive(Debug, Default)]
struct ViewLoadHealth {
    disk_reserve: BTreeMap<String, crate::view_storage_reserve::DiskReserveCondition>,
    by_cause: BTreeMap<(String, String), u64>,
    total: u64,
    last: Option<ViewLoadFailure>,
    /// Last error-level log per plane with the repeats suppressed since.
    log_state: BTreeMap<String, (Option<Instant>, u64)>,
}

/// Why a root's view publications keep failing; see
/// [`ViewHealthSnapshot::publication_failure`].
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ViewPublicationFailure {
    /// `missing_payload` when the view names a payload the family blob store
    /// no longer holds, otherwise `publication_failed`.
    pub reason: &'static str,
    /// Failed attempts in the streak, including waits for the semantic index.
    pub repeat_count: usize,
    /// The latest attempt's error.
    pub error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_retry_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ViewSkippedPath {
    pub path: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Tier2HealthSnapshot {
    /// `building` while an `aft_inspect` run of a whole-repository analysis
    /// is in flight, otherwise `ready`, or `disabled` when inspect cannot run
    /// those analyses for this root. Nothing refreshes them in the
    /// background, so there is no stale state or refresh deadline to report.
    pub status: &'static str,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SuspendedDomainHealthSnapshot {
    pub domain: String,
    pub reason: String,
    pub death_count: u64,
    pub age_s: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RootHealthSnapshot {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_unavailable_reason: Option<&'static str>,
    pub project_root: String,
    pub actor_count: usize,
    pub state: RootHealthState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub search_index: Option<HealthComponentSnapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub semantic_index: Option<SemanticHealthComponentSnapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callgraph_store: Option<HealthComponentSnapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callgraph_repair_entries_60s: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callgraph_commits_60s: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callgraph_pages_or_bytes_written_60s: Option<u64>,
    /// Checkout view loads that failed for this project since this process
    /// started, keyed `"plane:cause"` (for example `"trigram:unavailable"`
    /// or `"callgraph:io"`). Busy loads are retried, not counted. A failed load
    /// fails the query that attempted it; there is no fallback walk or
    /// legacy-index answer. Kept as history:
    /// a later successful load clears the degraded flag but never resets these.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub view_load_failures: BTreeMap<String, u64>,
    /// The most recent view load failure, if any load has failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub view_load_failure_last: Option<ViewLoadFailure>,
    /// Active measured disk-reserve refusals, separate from view-load bugs.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub view_storage_reserve: BTreeMap<String, crate::view_storage_reserve::DiskReserveCondition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tier2: Option<Tier2HealthSnapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bash: Option<BgTaskHealthCounts>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub suspended_domains: Vec<SuspendedDomainHealthSnapshot>,
    /// The data layer's pending edits and failures for this root's
    /// checkout; present only while the data layer runs, when
    /// `search_index` and `callgraph_store` describe its planes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_store: Option<crate::data_layer::surfaces::DataStoreHealth>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RootHealthSummary {
    cache_unavailable: Option<&'static str>,
    state: RootHealthState,
    search_index_status: Option<&'static str>,
    semantic_index: Option<SemanticHealthComponentSnapshot>,
    callgraph_store_status: Option<&'static str>,
    view_load_failures: BTreeMap<String, u64>,
    view_load_failure_last: Option<ViewLoadFailure>,
    view_storage_reserve: BTreeMap<String, crate::view_storage_reserve::DiskReserveCondition>,
    tier2_status: Option<&'static str>,
    bash: Option<BgTaskHealthCounts>,
    suspended_domains: Vec<SuspendedDomainHealthSnapshot>,
    data_store: Option<crate::data_layer::surfaces::DataStoreHealth>,
}

impl RootHealthSummary {
    fn busy() -> Self {
        Self {
            state: RootHealthState::Busy,
            cache_unavailable: None,
            search_index_status: None,
            semantic_index: None,
            callgraph_store_status: None,
            view_load_failures: BTreeMap::new(),
            view_load_failure_last: None,
            view_storage_reserve: BTreeMap::new(),
            tier2_status: None,
            bash: None,
            suspended_domains: Vec::new(),
            data_store: None,
        }
    }

    pub(crate) fn is_busy(&self) -> bool {
        matches!(self.state, RootHealthState::Busy)
    }

    pub(crate) fn is_fully_ready(&self) -> bool {
        // `off` is the data layer's plane on a root nothing has queried yet:
        // nothing is built and nothing is pending.
        let component_is_satisfied = |status: &str| matches!(status, "ready" | "disabled" | "off");
        matches!(self.state, RootHealthState::Ready)
            && self.view_storage_reserve.is_empty()
            && self.search_index_status.is_some_and(component_is_satisfied)
            && self
                .semantic_index
                .as_ref()
                .is_some_and(|semantic| component_is_satisfied(semantic.status))
            && self
                .callgraph_store_status
                .is_some_and(component_is_satisfied)
            && self.tier2_status.is_some_and(component_is_satisfied)
    }

    pub(crate) fn into_snapshot(self, project_root: &Path) -> RootHealthSnapshot {
        if self.is_busy() {
            return RootHealthSnapshot::busy(project_root);
        }
        RootHealthSnapshot {
            cache_unavailable_reason: self.cache_unavailable,
            project_root: project_root.display().to_string(),
            actor_count: 1,
            state: self.state,
            search_index: self
                .search_index_status
                .map(|status| HealthComponentSnapshot { status }),
            semantic_index: self.semantic_index,
            callgraph_store: self
                .callgraph_store_status
                .map(|status| HealthComponentSnapshot { status }),
            callgraph_repair_entries_60s: None,
            callgraph_commits_60s: None,
            callgraph_pages_or_bytes_written_60s: None,
            view_load_failures: self.view_load_failures,
            view_load_failure_last: self.view_load_failure_last,
            view_storage_reserve: self.view_storage_reserve,
            tier2: self
                .tier2_status
                .map(|status| Tier2HealthSnapshot { status }),
            bash: self.bash,
            suspended_domains: self.suspended_domains,
            data_store: self.data_store,
        }
    }
}

impl RootHealthSnapshot {
    fn busy(project_root: &Path) -> Self {
        Self {
            cache_unavailable_reason: None,
            project_root: project_root.display().to_string(),
            actor_count: 1,
            state: RootHealthState::Busy,
            search_index: None,
            semantic_index: None,
            callgraph_store: None,
            callgraph_repair_entries_60s: None,
            callgraph_commits_60s: None,
            callgraph_pages_or_bytes_written_60s: None,
            view_load_failures: BTreeMap::new(),
            view_load_failure_last: None,
            view_storage_reserve: BTreeMap::new(),
            tier2: None,
            bash: None,
            suspended_domains: Vec::new(),
            data_store: None,
        }
    }

    pub fn is_fully_ready(&self) -> bool {
        // `off` as in `RootHealthSummary::is_fully_ready`.
        let component_is_satisfied = |status: &HealthComponentSnapshot| {
            matches!(status.status, "ready" | "disabled" | "off")
        };
        let tier2_is_satisfied =
            |tier2: &Tier2HealthSnapshot| matches!(tier2.status, "ready" | "disabled");

        matches!(self.state, RootHealthState::Ready)
            && self.view_storage_reserve.is_empty()
            && self
                .search_index
                .as_ref()
                .is_some_and(component_is_satisfied)
            && self
                .semantic_index
                .as_ref()
                .is_some_and(|semantic| matches!(semantic.status, "ready" | "disabled"))
            && self
                .callgraph_store
                .as_ref()
                .is_some_and(component_is_satisfied)
            && self.tier2.as_ref().is_some_and(tier2_is_satisfied)
    }
}

pub struct StatusEmitter {
    state: Arc<StatusEmitterState>,
    notify: mpsc::Sender<Weak<StatusEmitterState>>,
    context: Mutex<Weak<AppContext>>,
    #[cfg(test)]
    pub(crate) snapshot_builds: AtomicUsize,
    #[cfg(test)]
    pub(crate) memory_census_builds: AtomicUsize,
}

/// The part of a context's status emitter that the process-wide debounce
/// thread reads when a window closes. That thread holds it only weakly between
/// signals, so it never keeps a dropped context's emitter alive.
struct StatusEmitterState {
    latest: Mutex<Option<PendingStatusSnapshot>>,
    progress_sender: SharedProgressSender,
    /// Whether this emitter's last push is still being built and sent.
    emitting: AtomicBool,
}

enum PendingStatusSnapshot {
    Ready(StatusPayload, Weak<AppContext>),
    Context(Weak<AppContext>),
}

#[derive(Clone, Debug, Default)]
struct ConfigureWarmState {
    generation: u64,
    key: Option<String>,
}

/// How long the health rollup reuses one index's memory walk while that
/// index's shape counters stay the same. A bound on how late an in-place
/// change that leaves every counter alone can show.
const HEALTH_MEMORY_REUSE_INTERVAL: Duration = Duration::from_secs(30);

/// The three resident-index estimates (semantic, trigram, symbols).
/// `measured_at` is when the oldest of them was taken.
#[derive(Clone, Debug)]
struct WalkedMemory {
    measured_at: Instant,
    /// The semantic walk before the live worker bytes are added.
    semantic: crate::memory::MemoryEstimate,
    trigram: crate::memory::MemoryEstimate,
    symbols: crate::memory::MemoryEstimate,
}

/// One index's last memory walk, with the shape counters read just before it
/// (see each type's `memory_shape`; `None` stands for a missing index).
#[derive(Clone, Debug)]
struct IndexMemoryWalk<S> {
    shape: S,
    measured_at: Instant,
    estimate: crate::memory::MemoryEstimate,
}

/// The health rollup's reusable walks, kept per index. Reads add files to the
/// symbol cache far more often than edits change the trigram or semantic
/// index. Keeping their estimates apart means a read refreshes only the
/// constant-time symbol totals rather than walking the other indexes.
#[derive(Clone, Debug, Default)]
struct HealthMemoryWalks {
    semantic: Option<IndexMemoryWalk<Option<[u64; 9]>>>,
    trigram: Option<IndexMemoryWalk<Option<[u64; 11]>>>,
    symbols: Option<IndexMemoryWalk<[u64; 5]>>,
}

/// Progress of the background walks that record which language servers apply
/// to the project (see `AppContext::refresh_project_applicability`), so a
/// caller that reports diagnostics state can wait for the current walk instead
/// of reporting "pending" while it runs.
#[derive(Default)]
struct ProjectApplicabilityProgress {
    /// The highest walk epoch that has finished, however it ended.
    settled: AtomicU64,
    gate: parking_lot::Mutex<ApplicabilityWalkGate>,
    /// Signalled when a walk settles or a test hold is released.
    changed: parking_lot::Condvar,
    refresh: parking_lot::Mutex<ApplicabilityRefreshState>,
    #[cfg(test)]
    refresh_spawns: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    scans: parking_lot::Mutex<ApplicabilityScanGate>,
    #[cfg(test)]
    scan_changed: parking_lot::Condvar,
}

#[derive(Default)]
struct ApplicabilityRefreshState {
    running: bool,
    pending: Option<ApplicabilityRefresh>,
}

struct ApplicabilityRefresh {
    config: Arc<Config>,
    epoch: u64,
    root_changed: bool,
}

#[cfg(test)]
#[derive(Default)]
struct ApplicabilityScanGate {
    held: bool,
    started: usize,
}

#[derive(Default)]
struct ApplicabilityWalkGate {
    /// Set by a test hold: walks pause before walking until it is released.
    held: bool,
    /// Callers currently inside `wait_for_project_applicability`.
    waiters: usize,
}

impl ProjectApplicabilityProgress {
    fn settle(&self, epoch: u64) {
        self.settled.fetch_max(epoch, Ordering::SeqCst);
        // Taking the lock after the store means a waiter that read the old
        // value under the lock is already parked on the condvar.
        let _gate = self.gate.lock();
        self.changed.notify_all();
    }

    fn pause_while_held(&self) {
        let mut gate = self.gate.lock();
        while gate.held {
            self.changed.wait(&mut gate);
        }
    }

    #[cfg(test)]
    fn note_scan_for_test(&self) {
        let mut scans = self.scans.lock();
        scans.started += 1;
        self.scan_changed.notify_all();
        while scans.held {
            self.scan_changed.wait(&mut scans);
        }
    }
}

/// Test hook: while this guard lives, every language-server applicability walk
/// of its context pauses before walking. Dropping it releases them.
#[doc(hidden)]
pub struct ProjectApplicabilityHold(Arc<ProjectApplicabilityProgress>);

impl Drop for ProjectApplicabilityHold {
    fn drop(&mut self) {
        self.0.gate.lock().held = false;
        self.0.changed.notify_all();
    }
}

#[cfg(test)]
thread_local! {
    /// Memory estimates on this thread that walked at least one of a root's
    /// resident indexes.
    static MEMORY_ESTIMATE_WALKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Walks of each resident index on this thread: semantic, trigram and
    /// symbol cache, in that order.
    static MEMORY_INDEX_WALKS: std::cell::RefCell<[usize; 3]> = const { std::cell::RefCell::new([0; 3]) };
    static MEMORY_WALK_CLOCK: std::cell::Cell<Option<Instant>> = const { std::cell::Cell::new(None) };
}

/// Override only this thread's memory-walk clock for a synchronous test refresh.
#[cfg(test)]
pub(crate) fn with_memory_walk_clock_for_test<T>(now: Instant, refresh: impl FnOnce() -> T) -> T {
    struct Reset(Option<Instant>);
    impl Drop for Reset {
        fn drop(&mut self) {
            MEMORY_WALK_CLOCK.with(|clock| clock.set(self.0));
        }
    }
    let _reset = Reset(MEMORY_WALK_CLOCK.with(|clock| clock.replace(Some(now))));
    refresh()
}

#[cfg(test)]
fn memory_walk_now_for_test() -> Instant {
    MEMORY_WALK_CLOCK.with(|clock| clock.get().unwrap_or_else(Instant::now))
}

#[cfg(test)]
pub(crate) fn memory_estimate_walks_for_test() -> usize {
    MEMORY_ESTIMATE_WALKS.with(std::cell::Cell::get)
}

/// Reuse one index's cached memory walk while its shape is unchanged and the
/// walk is younger than [`HEALTH_MEMORY_REUSE_INTERVAL`]; otherwise walk it
/// again. `shape` is `None` when the index was locked. Returns the estimate,
/// when it was measured, and whether this call walked.
fn reuse_or_walk_index_memory<S: PartialEq>(
    cached: &mut Option<IndexMemoryWalk<S>>,
    shape: Option<S>,
    walk: impl FnOnce() -> crate::memory::MemoryEstimate,
) -> (crate::memory::MemoryEstimate, Instant, bool) {
    if let (Some(shape), Some(previous)) = (shape.as_ref(), cached.as_ref()) {
        #[cfg(not(test))]
        let age = previous.measured_at.elapsed();
        #[cfg(test)]
        let age = memory_walk_now_for_test().saturating_duration_since(previous.measured_at);
        if &previous.shape == shape && age < HEALTH_MEMORY_REUSE_INTERVAL {
            return (previous.estimate.clone(), previous.measured_at, false);
        }
    }
    #[cfg(not(test))]
    let measured_at = Instant::now();
    #[cfg(test)]
    let measured_at = memory_walk_now_for_test();
    let estimate = walk();
    // A walk that found its index locked is never reused, so the next rollup
    // tries again rather than serving `busy` for 30 s.
    *cached = shape
        .filter(|_| estimate.status != "busy")
        .map(|shape| IndexMemoryWalk {
            shape,
            measured_at,
            estimate: estimate.clone(),
        });
    (estimate, measured_at, true)
}

#[derive(Debug)]
struct ConfigurePhaseTiming {
    phase: &'static str,
    started_at: Instant,
    /// Finished phases with their durations and the named steps inside each.
    completed: Vec<(&'static str, Duration, Vec<(&'static str, Duration)>)>,
    /// The named step running inside `phase`, if the phase marks steps.
    step: Option<(&'static str, Instant)>,
    /// Finished steps of `phase`.
    steps: Vec<(&'static str, Duration)>,
}

impl ConfigurePhaseTiming {
    /// Ends the running step of the current phase at `now`, if one runs.
    fn finish_step(&mut self, now: Instant) {
        if let Some((step, started_at)) = self.step.take() {
            self.steps
                .push((step, now.saturating_duration_since(started_at)));
        }
    }
}

/// `name=Nms`, followed by `(step=Nms,...)` when the phase marked steps.
fn format_configure_phase(
    phase: &str,
    elapsed: Duration,
    steps: impl Iterator<Item = (&'static str, Duration)>,
) -> String {
    let steps = steps
        .map(|(step, elapsed)| format!("{step}={}ms", elapsed.as_millis()))
        .collect::<Vec<_>>();
    if steps.is_empty() {
        format!("{phase}={}ms", elapsed.as_millis())
    } else {
        format!("{phase}={}ms({})", elapsed.as_millis(), steps.join(","))
    }
}

impl Default for ConfigurePhaseTiming {
    fn default() -> Self {
        Self {
            phase: "idle",
            started_at: Instant::now(),
            completed: Vec::new(),
            step: None,
            steps: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum WatcherDrainApplyPhase {
    #[default]
    PendingTier2,
    PendingIndexes,
    SymbolCache,
    SearchIndex,
    SemanticIndex,
    LspDiagnostics,
    Complete,
}

#[derive(Debug, Default)]
pub(crate) enum WatcherDrainPhase {
    #[default]
    Collect,
    Apply {
        stage: WatcherDrainApplyPhase,
        paths: VecDeque<PathBuf>,
        remaining: usize,
        oversized_inline_batch: bool,
    },
}

#[derive(Debug)]
enum DeletedRootCleanup {
    Pending(u64),
    Inconclusive(u64),
    Complete(u64),
}

pub(crate) struct PreparedGitignoreRebuild {
    inputs: gitignore_state::IgnoreInputSnapshot,
    matcher: Option<Arc<ignore::gitignore::Gitignore>>,
    change: IgnoreRuleChange,
}

#[cfg(test)]
pub(crate) struct IgnoreRescanGate {
    pub(crate) root: PathBuf,
    pub(crate) reached: crossbeam_channel::Sender<()>,
    pub(crate) release: crossbeam_channel::Receiver<()>,
}

#[cfg(test)]
pub(crate) static IGNORE_RESCAN_GATE: std::sync::OnceLock<
    parking_lot::Mutex<Option<IgnoreRescanGate>>,
> = std::sync::OnceLock::new();

#[derive(Debug)]
pub(crate) struct WatcherDrainSliceState {
    pub(crate) configure_generation: u64,
    /// Content identity of the configuration this continuation was built
    /// under. A lifecycle-only generation change (route unbind/rebind with an
    /// equivalent config) preserves the continuation by REBASING it onto the
    /// new generation; a content change discards it (the new configuration
    /// rebuilds artifacts wholesale).
    pub(crate) configure_content_generation: u64,
    pub(crate) phase: WatcherDrainPhase,
    /// Distinguishes a failed LSP lock attempt from entering that phase after
    /// an earlier phase used up the slice budget.
    pub(crate) lsp_diagnostics_lock_contended: bool,
    pub(crate) pending_paths: VecDeque<PathBuf>,
    pub(crate) ignore_changed: bool,
    pub(crate) ignore_refresh_due: Option<Instant>,
    pub(crate) ignore_changed_paths: Vec<PathBuf>,
    pub(crate) ignore_changed_path_count: usize,
    pub(crate) rescan_required: bool,
    pub(crate) rescan_ignore_checked: bool,
    pub(crate) ignore_scan: Option<crossbeam_channel::Receiver<PreparedGitignoreRebuild>>,
    pub(crate) rescan_reason: crate::watcher_filter::RescanReason,
    pub(crate) status_changed: bool,
    pub(crate) scheduler_changed_path_count: usize,
    pub(crate) semantic_refresh_paths: Vec<PathBuf>,
    pub(crate) path_slice_count: usize,
}

/// File changes the call graph has not taken yet, as root-relative paths with
/// `/` separators. See [`AppContext::pending_changes_for_query`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CallgraphUnappliedChanges {
    /// Files whose changes the root's call-graph store has not applied.
    pub store_paths: BTreeSet<String>,
    /// A disk comparison has not yet established which store files changed.
    pub whole_store: bool,
    /// Files whose changes the checkout's published view has not installed.
    pub view_paths: BTreeSet<String>,
    /// The view is republishing the whole checkout, so which files changed
    /// is not known.
    pub whole_checkout: bool,
}

impl CallgraphUnappliedChanges {
    pub fn is_empty(&self) -> bool {
        self.store_paths.is_empty()
            && self.view_paths.is_empty()
            && !self.whole_checkout
            && !self.whole_store
    }
}

/// File changes an answer may not reflect, captured just before the answer is
/// computed: `watcher` says whether unsorted watcher events exist, and
/// `callgraph` names files the call graph has not applied. See
/// [`AppContext::pending_changes_for_query`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct QueryPendingChanges {
    /// The dispatch channel or the watcher drain still holds file changes.
    pub watcher: bool,
    /// Files the call graph has not taken yet; empty unless requested.
    pub callgraph: CallgraphUnappliedChanges,
}

/// Points between two stage reads of [`AppContext::pending_changes_for_query`]
/// where a test can move a file one stage downstream.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PendingCaptureBoundary {
    /// After the flush-window set, before the dispatch channel and drain.
    AfterFlushWindow,
}

#[cfg(test)]
type PendingCaptureHook = Box<dyn FnMut(PendingCaptureBoundary)>;

#[cfg(test)]
thread_local! {
    static PENDING_CAPTURE_HOOK: std::cell::RefCell<Option<PendingCaptureHook>> =
        const { std::cell::RefCell::new(None) };
}

/// Run `hook` at each capture boundary reached on this thread until the
/// returned guard is dropped.
#[cfg(test)]
pub(crate) fn install_pending_capture_hook_for_test(
    hook: impl FnMut(PendingCaptureBoundary) + 'static,
) -> PendingCaptureHookGuard {
    PENDING_CAPTURE_HOOK.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
    PendingCaptureHookGuard
}

#[cfg(test)]
pub(crate) struct PendingCaptureHookGuard;

#[cfg(test)]
impl Drop for PendingCaptureHookGuard {
    fn drop(&mut self) {
        PENDING_CAPTURE_HOOK.with(|slot| *slot.borrow_mut() = None);
    }
}

#[cfg(test)]
fn pending_capture_boundary_for_test(boundary: PendingCaptureBoundary) {
    let hook = PENDING_CAPTURE_HOOK.with(|slot| slot.borrow_mut().take());
    if let Some(mut hook) = hook {
        hook(boundary);
        PENDING_CAPTURE_HOOK.with(|slot| {
            let mut slot = slot.borrow_mut();
            if slot.is_none() {
                *slot = Some(hook);
            }
        });
    }
}

/// Counts a watcher drain while it runs. The second counter advances when a
/// drain starts and again when it ends, so a pending-changes capture can tell
/// that a drain moved work while the capture was reading (see
/// [`AppContext::watcher_query_has_pending_changes`]).
pub(crate) struct WatcherDrainGuard<'a> {
    in_flight: &'a AtomicUsize,
    activity: &'a AtomicU64,
}

impl Drop for WatcherDrainGuard<'_> {
    fn drop(&mut self) {
        self.in_flight.fetch_sub(1, Ordering::AcqRel);
        self.activity.fetch_add(1, Ordering::AcqRel);
    }
}

/// Pending watcher-derived reconciliation state taken out of the context for
/// a transactional TTL teardown: committed (dropped) once eviction succeeds,
/// restored when a secondary blocker aborts the eviction.
pub(crate) struct PendingReconciliationState {
    tier2: BTreeSet<PathBuf>,
    semantic: BTreeSet<PathBuf>,
    corpus_refresh: bool,
}

/// Put one taken pending-path set back into the context's live set.
///
/// The idle reaper takes and restores these sets on every sweep for each
/// unbound root an artifact blocker keeps resident, on the subc frame loop.
/// Usually nothing was recorded while the set was out, so the taken set moves
/// back whole without comparing a single path. Re-inserting path by path cost
/// a full-path comparison per tree level for every pending path, every sweep.
/// When the watcher did record paths meanwhile, the two sorted sets are merged
/// in one linear pass.
fn restore_pending_paths(pending: &mut BTreeSet<PathBuf>, mut taken: BTreeSet<PathBuf>) {
    #[cfg(test)]
    if pending_restore_work::legacy_enabled() {
        // The path-by-path re-insertion this function replaced, kept so tests
        // can compare reap decisions and work against it.
        pending_restore_work::note_merged(taken.len());
        pending.extend(taken);
        return;
    }
    if taken.is_empty() {
        return;
    }
    if pending.is_empty() {
        *pending = taken;
        return;
    }
    #[cfg(test)]
    pending_restore_work::note_merged(pending.len() + taken.len());
    pending.append(&mut taken);
}

/// Counts the pending paths [`restore_pending_paths`] had to merge one by one
/// on the calling thread, so tests can assert that a reap sweep's restore
/// work does not grow with the number of roots or pending paths.
#[cfg(test)]
pub(crate) mod pending_restore_work {
    use std::cell::Cell;

    thread_local! {
        static MERGED: Cell<usize> = const { Cell::new(0) };
        static LEGACY: Cell<bool> = const { Cell::new(false) };
    }

    pub(super) fn note_merged(paths: usize) {
        MERGED.with(|merged| merged.set(merged.get() + paths));
    }

    /// Paths merged one by one on this thread since the last call.
    pub(crate) fn take_merged() -> usize {
        MERGED.with(|merged| merged.replace(0))
    }

    pub(super) fn legacy_enabled() -> bool {
        LEGACY.with(Cell::get)
    }

    /// Run `f` with restores on this thread re-inserting every taken path
    /// one by one, as they did before restores moved whole sets back.
    pub(crate) fn with_legacy_restore<R>(f: impl FnOnce() -> R) -> R {
        struct Reset(bool);
        impl Drop for Reset {
            fn drop(&mut self) {
                LEGACY.with(|legacy| legacy.set(self.0));
            }
        }
        let _reset = Reset(LEGACY.with(|legacy| legacy.replace(true)));
        f()
    }
}

/// Why [`AppContext::artifact_eviction_blocker`] refused eviction. The idle
/// reaper counts retained unbound roots per kind in its health census.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum ArtifactEvictionBlocker {
    /// A configured standing root keeps its artifacts resident on purpose.
    StandingRoot,
    /// A search, callgraph, or semantic build receiver is still installed.
    BuildInFlight,
    /// A semantic build or incremental refresh is recorded as running.
    SemanticRefresh,
    /// An inspect Tier-2 job is counted as in flight.
    InspectTier2,
    /// A background bash task started from this root is still running.
    BashRunning,
    /// Watcher-derived paths still await reconciliation into an artifact.
    PendingReconciliation,
    /// A lock the check reads was held by another thread, so the answer is
    /// not available without waiting. Only the non-blocking check used on the
    /// subc frame loop reports this; the reaper retries on its next sweep.
    LockContended,
}

impl ArtifactEvictionBlocker {
    /// Whether a teardown job can clear this blocker on an unbound root:
    /// dropping inspect completions that nothing drains while the root is
    /// unbound. The other blockers are either intentional (standing roots,
    /// running bash tasks) or clear on their own.
    pub(crate) fn clearable_by_unbound_teardown(self) -> bool {
        matches!(self, Self::InspectTier2)
    }
}

/// Test gate that parks the unbound-teardown persist of one root after its
/// unbound check, so a test can rebind the root while the persist is running.
#[cfg(test)]
struct UnboundTeardownPersistGate {
    root: PathBuf,
    reached: crossbeam_channel::Sender<()>,
    release: crossbeam_channel::Receiver<()>,
}

#[cfg(test)]
static UNBOUND_TEARDOWN_PERSIST_GATE: std::sync::OnceLock<
    parking_lot::Mutex<Option<UnboundTeardownPersistGate>>,
> = std::sync::OnceLock::new();

#[cfg(test)]
fn wait_on_unbound_teardown_persist_gate_for_test(root: Option<&Path>) {
    let gate = UNBOUND_TEARDOWN_PERSIST_GATE.get().and_then(|slot| {
        slot.lock()
            .as_ref()
            .filter(|gate| Some(gate.root.as_path()) == root)
            .map(|gate| (gate.reached.clone(), gate.release.clone()))
    });
    if let Some((reached, release)) = gate {
        let _ = reached.send(());
        let _ = release.recv_timeout(Duration::from_secs(30));
    }
}

impl WatcherDrainSliceState {
    pub(crate) fn new(configure_generation: u64, configure_content_generation: u64) -> Self {
        Self {
            configure_generation,
            configure_content_generation,
            phase: WatcherDrainPhase::Collect,
            lsp_diagnostics_lock_contended: false,
            pending_paths: VecDeque::new(),
            ignore_changed: false,
            ignore_refresh_due: None,
            ignore_changed_paths: Vec::new(),
            ignore_changed_path_count: 0,
            rescan_required: false,
            rescan_ignore_checked: false,
            ignore_scan: None,
            rescan_reason: crate::watcher_filter::RescanReason::Unknown,
            status_changed: false,
            scheduler_changed_path_count: 0,
            semantic_refresh_paths: Vec::new(),
            path_slice_count: 0,
        }
    }

    pub(crate) fn retire_ignored_paths(&mut self, keep: &impl Fn(&PathBuf) -> bool) -> usize {
        let mut dropped = 0;
        let before = self.pending_paths.len();
        self.pending_paths.retain(keep);
        dropped += before - self.pending_paths.len();
        if let WatcherDrainPhase::Apply {
            paths, remaining, ..
        } = &mut self.phase
        {
            // The deque rotates processed paths behind the unprocessed prefix.
            // Preserve that boundary when retiring a continuation between slices.
            let mut position = 0;
            let mut remaining_kept = 0;
            let before = paths.len();
            paths.retain(|path| {
                let retain = keep(path);
                if position < *remaining && retain {
                    remaining_kept += 1;
                }
                position += 1;
                retain
            });
            *remaining = remaining_kept;
            dropped += before - paths.len();
        }
        let before = self.semantic_refresh_paths.len();
        self.semantic_refresh_paths.retain(keep);
        dropped + before - self.semantic_refresh_paths.len()
    }

    pub(crate) fn has_pending_work(&self) -> bool {
        !matches!(self.phase, WatcherDrainPhase::Collect)
            || !self.pending_paths.is_empty()
            || self.ignore_changed
            || self.rescan_required
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ConfigureMaintenanceJob {
    pub(crate) generation: u64,
    pub(crate) root_path: PathBuf,
    pub(crate) canonical_cache_root: PathBuf,
    pub(crate) harness: Harness,
    pub(crate) storage_root: PathBuf,
    pub(crate) harness_dir: PathBuf,
    pub(crate) session_id: String,
    pub(crate) root_too_broad: bool,
    pub(crate) format_tool_cache_clear_needed: bool,
    pub(crate) run_bash_replay: bool,
    pub(crate) configure_database_runtime: bool,
    pub(crate) refresh_project_runtime: bool,
    pub(crate) sync_bash_compress_flag: bool,
    pub(crate) reset_filter_registry: bool,
    pub(crate) clear_failed_spawns: bool,
    /// Advance search disk-publication epochs only after the configure
    /// acknowledgement is sent. Updating an epoch may wait for a writer already
    /// committing, so the initial route bind must not perform this work.
    pub(crate) supersede_search_artifact_persistence: bool,
    /// Keep the adopted semantic worker's artifact-publication epoch valid so it
    /// can still publish its result while unrelated configure work replaces the
    /// other artifact lanes.
    pub(crate) supersede_semantic_artifact_persistence: bool,
}

impl StatusEmitter {
    fn new(progress_sender: SharedProgressSender) -> Self {
        Self {
            state: Arc::new(StatusEmitterState {
                latest: Mutex::new(None),
                progress_sender,
                emitting: AtomicBool::new(false),
            }),
            notify: status_debounce_sender(),
            context: Mutex::new(Weak::new()),
            #[cfg(test)]
            snapshot_builds: AtomicUsize::new(0),
            #[cfg(test)]
            memory_census_builds: AtomicUsize::new(0),
        }
    }

    pub fn signal(&self, snapshot: StatusPayload) {
        let context = self
            .context
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Ok(mut latest) = self.state.latest.lock() {
            *latest = Some(PendingStatusSnapshot::Ready(snapshot, context));
        }
        let _ = self.notify.send(Arc::downgrade(&self.state));
    }

    fn signal_context(&self, context: Weak<AppContext>) {
        if let Ok(mut latest) = self.state.latest.lock() {
            *latest = Some(PendingStatusSnapshot::Context(context));
        }
        let _ = self.notify.send(Arc::downgrade(&self.state));
    }
}

/// The sender into the one debounce thread that serves every context's status
/// emitter. A thread per context would sit blocked for as long as the context
/// lives, and the daemon keeps a context for every root it has served, idle
/// or not.
fn status_debounce_sender() -> mpsc::Sender<Weak<StatusEmitterState>> {
    static SENDER: std::sync::LazyLock<mpsc::Sender<Weak<StatusEmitterState>>> =
        std::sync::LazyLock::new(|| {
            let (notify, rx) = mpsc::channel();
            std::thread::Builder::new()
                .name("aft-status-debounce".to_string())
                .spawn(move || status_debounce_loop(rx))
                .expect("spawn status debounce thread");
            notify
        });
    SENDER.clone()
}

/// Coalesce status signals per emitter: the first signal opens a window of
/// `STATUS_DEBOUNCE_MS`, later signals inside it only replace the pending
/// snapshot, and the window's close pushes the latest one. Windows of
/// different emitters run independently. Returns once every sender is gone.
///
/// This thread only keeps time. Building a push can read the project's
/// filesystem, which may sit on a slow or unavailable mount, so each push is
/// built and sent on a thread of its own: one stalled project then delays
/// only its own status. An emitter whose previous push is still running gets
/// its window re-armed, so its latest snapshot follows once that push ends
/// and it never has more than one push thread.
fn status_debounce_loop(rx: mpsc::Receiver<Weak<StatusEmitterState>>) {
    // Every window has the same length, so arrival order is deadline order.
    let mut windows: VecDeque<(Instant, Weak<StatusEmitterState>)> = VecDeque::new();
    loop {
        let now = Instant::now();
        while windows
            .front()
            .is_some_and(|(deadline, _)| *deadline <= now)
        {
            let Some((_, state)) = windows.pop_front() else {
                break;
            };
            // A context dropped inside its window pushes nothing.
            let Some(strong) = state.upgrade() else {
                continue;
            };
            if strong.emitting.swap(true, Ordering::AcqRel) {
                windows.push_back((now + Duration::from_millis(STATUS_DEBOUNCE_MS), state));
                continue;
            }
            spawn_status_emit(strong);
        }
        let signalled = match windows.front() {
            Some((deadline, _)) => {
                match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                    Ok(state) => state,
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => return,
                }
            }
            None => match rx.recv() {
                Ok(state) => state,
                Err(_) => return,
            },
        };
        if !windows
            .iter()
            .any(|(_, open)| Weak::ptr_eq(open, &signalled))
        {
            windows.push_back((
                Instant::now() + Duration::from_millis(STATUS_DEBOUNCE_MS),
                signalled,
            ));
        }
    }
}

/// Hold a status emission for `root` until the test releases it, so a test
/// can stand in for a project whose filesystem stalls the status build.
#[cfg(test)]
pub(crate) struct StatusEmitGateForTest {
    root: PathBuf,
    pub(crate) reached: crossbeam_channel::Receiver<()>,
    pub(crate) release: crossbeam_channel::Sender<()>,
}

#[cfg(test)]
type StatusEmitGates = parking_lot::Mutex<
    std::collections::HashMap<
        PathBuf,
        (
            crossbeam_channel::Sender<()>,
            crossbeam_channel::Receiver<()>,
        ),
    >,
>;

#[cfg(test)]
fn status_emit_gates() -> &'static StatusEmitGates {
    static GATES: std::sync::OnceLock<StatusEmitGates> = std::sync::OnceLock::new();
    GATES.get_or_init(Default::default)
}

#[cfg(test)]
pub(crate) fn gate_status_emit_for_test(root: &Path) -> StatusEmitGateForTest {
    let (reached_tx, reached) = crossbeam_channel::unbounded();
    let (release, release_rx) = crossbeam_channel::unbounded();
    status_emit_gates()
        .lock()
        .insert(root.to_path_buf(), (reached_tx, release_rx));
    StatusEmitGateForTest {
        root: root.to_path_buf(),
        reached,
        release,
    }
}

#[cfg(test)]
impl Drop for StatusEmitGateForTest {
    fn drop(&mut self) {
        status_emit_gates().lock().remove(&self.root);
        let _ = self.release.send(());
    }
}

#[cfg(test)]
fn wait_on_status_emit_gate_for_test(context: &AppContext) {
    let Some(root) = context.config().project_root.clone() else {
        return;
    };
    let gate = status_emit_gates().lock().get(&root).cloned();
    if let Some((reached, release)) = gate {
        let _ = reached.send(());
        let _ = release.recv_timeout(Duration::from_secs(30));
    }
}

fn spawn_status_emit(state: Arc<StatusEmitterState>) {
    let spawned = std::thread::Builder::new()
        .name("aft-status-emit".to_string())
        .spawn({
            let state = Arc::clone(&state);
            move || {
                emit_pending_status(&state);
                state.emitting.store(false, Ordering::Release);
            }
        });
    if spawned.is_err() {
        // No thread to spare: push inline rather than drop the status.
        emit_pending_status(&state);
        state.emitting.store(false, Ordering::Release);
    }
}

fn emit_pending_status(state: &StatusEmitterState) {
    let snapshot = state
        .latest
        .lock()
        .ok()
        .and_then(|mut latest| latest.take());
    let Some(snapshot) = snapshot else { return };
    let sender = state
        .progress_sender
        .lock()
        .ok()
        .and_then(|sender| sender.clone());
    let Some(sender) = sender else { return };
    // Census work happens only for the surviving signal, outside the
    // emitter lock. Upgrade at emission so queued signals cannot keep a
    // retired actor (and its watchers/servers) alive.
    let snapshot = match snapshot {
        PendingStatusSnapshot::Ready(snapshot, context) => match context.upgrade() {
            Some(context) => {
                #[cfg(test)]
                wait_on_status_emit_gate_for_test(&context);
                crate::status_push::project(&context, snapshot)
            }
            None => snapshot,
        },
        PendingStatusSnapshot::Context(context) => {
            let Some(context) = context.upgrade() else {
                return;
            };
            #[cfg(test)]
            wait_on_status_emit_gate_for_test(&context);
            let Some(snapshot) = context.try_build_status_push_snapshot() else {
                // Keep the previous bar; the next producer signal retries.
                return;
            };
            snapshot
        }
    };
    sender(PushFrame::StatusChanged(StatusChangedFrame::new(
        None, snapshot,
    )));
}
use crate::cache_freshness::FileFreshness;
use crate::semantic_index::{EmbeddingEntry, SemanticIndex};

/// The refresh circuit only trips once an index exists and later refreshes
/// fail. A first build against an unreachable backend never gets that far: it
/// parks in a retry loop that records its outage per project root instead.
/// Every status surface reads backend health through this fold so a cold build
/// that cannot reach its backend reports `backend_unavailable` with the real
/// reason rather than an endless "loading"/"rebuilding".
fn with_cold_build_backend_health(
    mut snapshot: EmbeddingBackendHealthSnapshot,
    root: Option<&Path>,
) -> EmbeddingBackendHealthSnapshot {
    if let Some(pause) = root.and_then(crate::synapse_embed::lane_pause_for_root) {
        snapshot.available = false;
        snapshot.last_error = Some(pause.reason);
        snapshot.paused_until_ms = Some(pause.paused_until_ms);
        snapshot.pause_cause = Some(pause.cause);
        snapshot.next_retry_ms = Some(pause.paused_until_ms);
        return snapshot;
    }
    let Some(build) = root.and_then(crate::semantic_index::embedding_backend_build_health) else {
        return snapshot;
    };
    let build_is_older = snapshot
        .since_ms
        .is_none_or(|since| build.since_ms <= since);
    if snapshot.available || build_is_older {
        snapshot.last_error = Some(build.last_error);
        snapshot.next_retry_ms = Some(build.next_retry_ms);
    }
    snapshot.since_ms = Some(
        snapshot
            .since_ms
            .map_or(build.since_ms, |since| since.min(build.since_ms)),
    );
    snapshot.available = false;
    snapshot
}

#[derive(Debug, Clone)]
struct SemanticBackendOutage {
    last_error: String,
    since_ms: u64,
    next_retry_ms: u64,
}

#[derive(Debug, Default)]
struct SemanticRefreshCircuit {
    consecutive_transient_failures: AtomicUsize,
    open: AtomicBool,
    probe_in_flight: AtomicBool,
    probe_ready: AtomicBool,
    probe_token: AtomicU64,
    backend_outage: RwLock<Option<SemanticBackendOutage>>,
}

fn semantic_health_unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct SemanticColdSeedResume {
    request_tier2: bool,
}

#[derive(Debug, Clone)]
pub enum SemanticRefreshRequest {
    Files {
        paths: Vec<PathBuf>,
    },
    /// Refresh the whole semantic corpus on the refresh worker. The worker owns
    /// the project walk so watcher/configure drains never do corpus-scale work
    /// on the single dispatch thread before scheduling embedding.
    Corpus,
}

#[derive(Debug)]
pub enum SemanticRefreshEvent {
    Started {
        paths: Vec<PathBuf>,
    },
    CorpusStarted {
        files: usize,
    },
    Completed {
        added_entries: Vec<EmbeddingEntry>,
        refused_rows: std::collections::BTreeMap<PathBuf, usize>,
        context_skipped: std::collections::BTreeMap<PathBuf, usize>,
        updated_metadata: Vec<(PathBuf, FileFreshness)>,
        completed_paths: Vec<PathBuf>,
    },
    CorpusCompleted {
        index: SemanticIndex,
        changed: usize,
        added: usize,
        deleted: usize,
        total_processed: usize,
    },
    Failed {
        paths: Vec<PathBuf>,
        error: String,
    },
    CorpusFailed {
        /// Files already selected by the corpus walk and still needing refresh.
        /// Recovery re-extracts this cheap file set instead of walking the corpus again.
        paths: Vec<PathBuf>,
        error: String,
    },
}

pub type SemanticRefreshWorkerSlot = Arc<Mutex<Option<std::thread::JoinHandle<()>>>>;

/// A finished artifact load that its loader has sent to the receiver but that
/// no completion drain has installed yet. `epoch` is the receiver epoch the
/// load was sent to and `bytes` is the estimate the census uses for the same
/// artifact once installed, computed once by the loader thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PendingInstall {
    pub(crate) epoch: u64,
    pub(crate) bytes: u64,
}

/// Records the size of a finished load between its hand-off and its install,
/// so the memory census never loses track of it. The loader records after its
/// send; the drain, a receiver retirement and a receiver replacement clear it.
/// The census counts the record only while the receiver it names is still the
/// current one and still holds undelivered events, so a record whose load was
/// already taken (the drain can take it between the send and the record) is
/// never counted.
#[derive(Debug, Default)]
pub(crate) struct PendingInstallSlot(parking_lot::Mutex<Option<PendingInstall>>);

impl PendingInstallSlot {
    fn get(&self) -> Option<PendingInstall> {
        *self.0.lock()
    }

    /// Remove and return the record when it belongs to receiver `epoch`.
    fn take_for_epoch(&self, epoch: u64) -> Option<PendingInstall> {
        let mut slot = self.0.lock();
        if slot.is_some_and(|pending| pending.epoch == epoch) {
            slot.take()
        } else {
            None
        }
    }
}

/// Log a finished load that is dropped without being installed, with the
/// estimate its loader recorded when there is one.
pub(crate) fn log_discarded_finished_load(
    plane: &str,
    root: Option<&Path>,
    bytes: Option<u64>,
    reason: &str,
) {
    let size = bytes.map_or_else(
        || "size not recorded".to_string(),
        |bytes| format!("~{:.1} MB", bytes as f64 / (1024.0 * 1024.0)),
    );
    crate::slog_info!(
        "discarded finished {} load for {} ({}) without installing it: {}",
        plane,
        root.map_or_else(
            || "<unconfigured root>".to_string(),
            |root| root.display().to_string()
        ),
        size,
        reason
    );
}

/// Log a finished load that is dropped with its receiver. `pending` is the
/// record for the receiver being dropped; nothing is logged when the receiver
/// held no finished load.
fn log_discarded_pending_install(
    plane: &str,
    root: Option<&Path>,
    pending: Option<PendingInstall>,
    reason: &str,
) {
    if let Some(pending) = pending {
        log_discarded_finished_load(plane, root, Some(pending.bytes), reason);
    }
}

/// The record of a finished load still sitting undelivered in `receiver`,
/// the receiver whose epoch is `epoch`.
fn undelivered_install<T>(
    slot: &PendingInstallSlot,
    epoch: u64,
    receiver: Option<&crossbeam_channel::Receiver<T>>,
) -> Option<PendingInstall> {
    slot.get().filter(|pending| {
        pending.epoch == epoch && receiver.is_some_and(|receiver| !receiver.is_empty())
    })
}

/// Like [`undelivered_install`], but also clears the record: the caller is
/// about to drop or drain the receiver.
fn take_undelivered_install<T>(
    slot: &PendingInstallSlot,
    epoch: u64,
    receiver: Option<&crossbeam_channel::Receiver<T>>,
) -> Option<PendingInstall> {
    let pending = slot.take_for_epoch(epoch)?;
    receiver
        .is_some_and(|receiver| !receiver.is_empty())
        .then_some(pending)
}

struct PathRestrictionContext {
    raw_root: PathBuf,
    resolved_root: PathBuf,
    path_for_resolution: PathBuf,
}

/// Marks explicit roots passed to search tools so their worker-only refusal
/// for a filesystem-root search can be handled at the shared path gate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WorkerSearchTool {
    Glob,
    Grep,
    AftSearch,
    AstGrepSearch,
}

/// Per-context memo for the configured project root used by containment checks.
///
/// `resolved_root` is the bare output from `fs::canonicalize`; it must not be
/// lexically normalized because it remains in the filesystem identity domain.
struct PathRestrictionRootMemo {
    configured_root: PathBuf,
    resolved_root: PathBuf,
}

/// Normalize a comparison path lexically, without touching the filesystem.
/// This does not validate containment; callers still compare the result
/// against the project root.
fn normalize_path(path: &Path) -> PathBuf {
    crate::path_identity::lexical_normalize(path)
}

#[cfg(test)]
mod lexical_comparison_tests {
    /// Unmatched leading `..` components must survive comparison normalization:
    /// dropping them makes a path that escapes upward look like a nearby child.
    #[test]
    fn comparison_adapter_preserves_relative_parent_components() {
        for (input, expected) in [("../../x", "../../x"), ("a/../../x", "../x")] {
            assert_eq!(
                super::normalize_path(std::path::Path::new(input)),
                std::path::PathBuf::from(expected),
                "{input}"
            );
        }
    }
}

fn relative_components_under_root(path: &Path) -> Option<PathBuf> {
    let mut components = path.components();
    match components.next()? {
        Component::Prefix(_) => match components.next()? {
            Component::RootDir => {}
            _ => return None,
        },
        Component::RootDir => {}
        _ => return None,
    }

    let mut relative = PathBuf::new();
    for component in components {
        relative.push(component.as_os_str());
    }
    Some(relative)
}

fn checkout_counterpart(original: &Path, checkout: &Path) -> Option<PathBuf> {
    let relative = relative_components_under_root(original)?;
    if relative.as_os_str().is_empty() {
        return None;
    }

    // Normalize before probing so a spelling such as /../secret never stats
    // the unnormalized path supplied by the caller.
    let normalized_original = normalize_path(original);
    match std::fs::symlink_metadata(&normalized_original) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        _ => return None,
    }

    let checkout = std::fs::canonicalize(checkout).ok()?;
    let candidate = normalize_path(&checkout.join(relative));
    if !candidate.starts_with(&checkout) {
        return None;
    }

    let canonical_candidate = std::fs::canonicalize(&candidate).ok()?;
    canonical_candidate
        .starts_with(&checkout)
        .then_some(canonical_candidate)
}

fn resolve_with_existing_ancestors(path: &Path) -> PathBuf {
    let mut existing = path.to_path_buf();
    let mut tail_segments = Vec::new();

    while !existing.exists() {
        if let Some(name) = existing.file_name() {
            tail_segments.push(name.to_owned());
        } else {
            break;
        }

        existing = match existing.parent() {
            Some(parent) => parent.to_path_buf(),
            None => break,
        };
    }

    let mut resolved = std::fs::canonicalize(&existing).unwrap_or(existing);
    for segment in tail_segments.into_iter().rev() {
        resolved.push(segment);
    }

    resolved
}

fn path_error_response(
    req_id: &str,
    path: &Path,
    resolved_root: &Path,
) -> crate::protocol::Response {
    crate::protocol::Response::error(
        req_id,
        "path_outside_root",
        format!(
            "path '{}' is outside the project root '{}'",
            path.display(),
            resolved_root.display()
        ),
    )
}

/// The main checkout of the linked Git worktree at `worktree`, when its
/// linkage checks out, in the policy's spelling (canonical, no Windows
/// verbatim prefix). Otherwise `None`, and the main checkout gets no
/// exemption.
///
/// A `.git` file is only a claim. The link is accepted when the worktree's
/// `.git` file names a directory directly under `<common>/worktrees/`, that
/// directory's `gitdir` file points back at this worktree's `.git` file, and
/// its `commondir` file resolves to `common`, the common Git directory, whose
/// own name is `.git`. The main checkout is the directory holding `common`.
fn verified_linked_main_root(worktree: &Path, common: &Path) -> Option<PathBuf> {
    let resolve = |base: &Path, spelled: &str| {
        let spelled = Path::new(spelled.trim());
        let path = if spelled.is_absolute() {
            spelled.to_path_buf()
        } else {
            base.join(spelled)
        };
        std::fs::canonicalize(path).ok()
    };
    let common = std::fs::canonicalize(common).ok()?;
    if common.file_name()? != ".git" {
        return None;
    }
    let worktree = std::fs::canonicalize(worktree).ok()?;
    let dot_git = worktree.join(".git");
    if !std::fs::symlink_metadata(&dot_git).ok()?.is_file() {
        return None;
    }
    let claim = std::fs::read_to_string(&dot_git).ok()?;
    let gitdir = resolve(&worktree, claim.trim().strip_prefix("gitdir:")?)?;
    if gitdir.parent()? != common.join("worktrees") {
        return None;
    }
    let back = resolve(
        &gitdir,
        &std::fs::read_to_string(gitdir.join("gitdir")).ok()?,
    )?;
    if back != std::fs::canonicalize(&dot_git).ok()? {
        return None;
    }
    let commondir = resolve(
        &gitdir,
        &std::fs::read_to_string(gitdir.join("commondir")).ok()?,
    )?;
    if commondir != common {
        return None;
    }
    Some(crate::path_identity::normalize_path(common.parent()?))
}

/// Whether `location` (absolute, with at least its parent resolved) is an
/// entry inside scratch that a task of `session` still owns.
/// A final symlink is never owned: the registry's check resolves the whole
/// path, which would judge the link's target rather than the link entry a
/// delete, move or replace acts on. A missing target is judged by its nearest
/// existing ancestor, which is already resolved because the parent is.
fn session_owned_scratch_location(
    registry: &BgTaskRegistry,
    session: &str,
    location: &Path,
) -> bool {
    let candidate = match std::fs::symlink_metadata(location) {
        Ok(metadata) if metadata.file_type().is_symlink() => return false,
        Ok(_) => Some(location),
        Err(_) => location.ancestors().skip(1).find(|parent| parent.exists()),
    };
    candidate.is_some_and(|candidate| registry.is_session_owned_scratch_path(session, candidate))
}

/// A GitHub address (`issue://…`, `pr://…`) is never a filesystem path. The
/// handlers that serve GitHub resources route those spellings before path
/// validation, so one arriving here was meant for GitHub and handed to a file
/// operation; accepting it would create or change `<root>/issue:/N` locally.
fn reject_github_address(req_id: &str, path: &Path) -> Result<(), crate::protocol::Response> {
    match path.to_str() {
        Some(raw) => crate::github_address::reject_github_resource_path(
            req_id,
            "file operation",
            raw,
        ),
        None => Ok(()),
    }
}

/// Walk `candidate` component-by-component. For any component that is a
/// symlink on disk, iteratively follow the full chain (up to 40 hops) and
/// reject if any hop's resolved target lies outside `resolved_root`.
///
/// This is the fallback path used when `fs::canonicalize` fails (e.g. on
/// Linux with broken symlink chains pointing to non-existent destinations).
/// On macOS `canonicalize` also fails for broken symlinks but the returned
/// `/var/...` tempdir paths diverge from `resolved_root`'s `/private/var/...`
/// form, so we must accept either form when deciding which symlinks to check.
fn reject_escaping_symlink(
    req_id: &str,
    original_path: &Path,
    candidate: &Path,
    resolved_root: &Path,
    raw_root: &Path,
) -> Result<(), crate::protocol::Response> {
    let mut current = PathBuf::new();

    for component in candidate.components() {
        current.push(component);

        let Ok(metadata) = std::fs::symlink_metadata(&current) else {
            continue;
        };

        if !metadata.file_type().is_symlink() {
            continue;
        }

        // Only check symlinks that live inside the project root. This skips
        // OS-level prefix symlinks (macOS /var → /private/var) that are not
        // inside our project directory and whose "escaping" is harmless.
        //
        // We compare against BOTH the canonicalized root (resolved_root, e.g.
        // /private/var/.../project) AND the raw root (e.g. /var/.../project)
        // because tempdir() returns raw paths while fs::canonicalize returns
        // the resolved form — and our `current` may be in either form.
        let inside_root = current.starts_with(resolved_root) || current.starts_with(raw_root);
        if !inside_root {
            continue;
        }

        iterative_follow_chain(req_id, original_path, &current, resolved_root)?;
    }

    Ok(())
}

/// Iteratively follow a symlink chain from `link` and reject if any hop's
/// resolved target is outside `resolved_root`. Depth-capped at 40 hops.
fn iterative_follow_chain(
    req_id: &str,
    original_path: &Path,
    start: &Path,
    resolved_root: &Path,
) -> Result<(), crate::protocol::Response> {
    let mut link = start.to_path_buf();
    let mut depth = 0usize;

    loop {
        if depth > 40 {
            return Err(path_error_response(req_id, original_path, resolved_root));
        }

        let target = match std::fs::read_link(&link) {
            Ok(t) => t,
            Err(_) => {
                // Can't read the link — treat as escaping to be safe.
                return Err(path_error_response(req_id, original_path, resolved_root));
            }
        };

        let resolved_target = if target.is_absolute() {
            normalize_path(&target)
        } else {
            let parent = link.parent().unwrap_or_else(|| Path::new(""));
            normalize_path(&parent.join(&target))
        };

        // Check boundary: use canonicalized target when available (handles
        // macOS /var → /private/var aliasing), fall back to the normalized
        // path when canonicalize fails (e.g. broken symlink on Linux).
        let canonical_target =
            std::fs::canonicalize(&resolved_target).unwrap_or_else(|_| resolved_target.clone());

        if !canonical_target.starts_with(resolved_root)
            && !resolved_target.starts_with(resolved_root)
        {
            return Err(path_error_response(req_id, original_path, resolved_root));
        }

        // If the target is itself a symlink, follow the next hop.
        match std::fs::symlink_metadata(&resolved_target) {
            Ok(meta) if meta.file_type().is_symlink() => {
                link = resolved_target;
                depth += 1;
            }
            _ => break, // Non-symlink or non-existent target — chain ends here.
        }
    }

    Ok(())
}

pub type LanguageProviderFactory = fn() -> Box<dyn LanguageProvider>;

pub fn default_language_provider_factory() -> Box<dyn LanguageProvider> {
    Box::new(TreeSitterProvider::new())
}

fn database_path_key(path: &Path) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return canonical;
    }
    let Some(parent) = path.parent() else {
        return path.to_path_buf();
    };
    let canonical_parent = std::fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf());
    path.file_name()
        .map(|name| canonical_parent.join(name))
        .unwrap_or_else(|| canonical_parent.join(path))
}

fn database_connection_busy() -> crate::db::OpenError {
    crate::db::OpenError::Sqlite(rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
        Some("process-shared database connection is in use".into()),
    ))
}

#[cfg(test)]
thread_local! {
    static DATABASE_SLOT_CONTENDED_FOR_TEST: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);
}

fn lock_database_slot<T>(
    slot: &parking_lot::Mutex<T>,
    mode: crate::db::OpenMode,
) -> Result<parking_lot::MutexGuard<'_, T>, crate::db::OpenError> {
    if mode == crate::db::OpenMode::SingleAttempt {
        if let Some(guard) = slot.try_lock() {
            return Ok(guard);
        }
        #[cfg(test)]
        DATABASE_SLOT_CONTENDED_FOR_TEST.with(|hook| {
            if let Some(hook) = hook.borrow_mut().take() {
                hook();
            }
        });
        // Even reading the installed handle briefly owns this mutex. Give those
        // readers time to leave without queuing behind an unbounded database open.
        return slot
            .try_lock_for(crate::db::TOOL_RETRY_BUSY_WAIT)
            .ok_or_else(database_connection_busy);
    }
    Ok(slot.lock())
}

/// Remembered root-confinement answers for scoped worker policy keys.
///
/// Confinement is monotonic (a later catalog fetch cannot remove it), so a
/// key found confined stays confined. An unconfined answer is also kept, so
/// an ordinary worker's calls skip aft.db and its lock; it can go stale only
/// through a confining save, and every such save goes through
/// `AppContext::remember_confined_policy_key`, which marks the key confined
/// and forgets every unconfined answer before the save commits. Unconfined
/// answers also belong to the aft.db handle they were read from: once the
/// context uses another database (a rebind to another storage directory, or
/// a reopen), they no longer count and the next call reads again. A new
/// process starts empty and reads the saved rows again.
#[derive(Default)]
struct PolicyConfinementMemo {
    confined: std::collections::HashSet<crate::db::remote_exec::PolicyKey>,
    unconfined: std::collections::HashSet<crate::db::remote_exec::PolicyKey>,
    /// The database the `unconfined` answers were read from.
    unconfined_db: Option<std::sync::Weak<Mutex<TrackedConnection>>>,
    /// Bumped by every confining save, so an unconfined answer read before
    /// one is not remembered after it.
    generation: u64,
    #[cfg(test)]
    store_reads: usize,
    /// Runs once, between a policy read from aft.db and remembering its
    /// answer, so a test can save a policy in that window.
    #[cfg(test)]
    after_read_for_test: Option<Box<dyn FnOnce(&AppContext) + Send>>,
    /// Runs after an inconclusive memory probe, before acquiring the database
    /// handle, so a test can release a contended slot at that exact boundary.
    #[cfg(test)]
    after_memory_miss_for_test: Option<Box<dyn FnOnce() + Send>>,
}

#[derive(Default)]
struct DatabaseRuntimeFailure {
    error: Option<String>,
    retry_at: Option<Instant>,
    backoff: Duration,
}

impl DatabaseRuntimeFailure {
    fn record(&mut self, error: String, busy: bool) {
        self.error = Some(error);
        if !busy {
            self.backoff = if self.backoff.is_zero() {
                Duration::from_secs(2)
            } else {
                (self.backoff * 2).min(Duration::from_secs(60))
            };
            // Measure from completion: even a slow failed open gets a full
            // pause before another call can attempt it.
            self.retry_at = Some(Instant::now() + self.backoff);
        }
    }
}

/// Process-global services shared by all project actors in this AFT process.
///
/// `App` owns only true process services. Per-root caches and the live
/// language provider instance stay in [`AppContext`].
pub struct App {
    /// One process-wide handle for the current AFT database. Every project
    /// actor points at this handle so roots do not open duplicate SQLite/WAL
    /// descriptors for the same database.
    db: parking_lot::Mutex<Option<(PathBuf, Arc<Mutex<TrackedConnection>>)>>,
    /// The authenticated module connection's endpoint, independent of model
    /// backend configuration. Consumer routes must use this same daemon.
    subc_connection_file: parking_lot::RwLock<Option<PathBuf>>,
    lifecycle_census: crate::lifecycle_census::LifecycleCensusCache,
    active_watchers: AtomicUsize,
    active_actor_roots: AtomicUsize,
    open_routes: AtomicUsize,
    lsp_child_registry: crate::lsp::child_registry::LspChildRegistry,
    stdout_writer: SharedStdoutWriter,
    provider_factory: LanguageProviderFactory,
    /// Weak actor references let status attribute process RSS across roots
    /// without making the process-global App own per-root caches.
    memory_contexts: parking_lot::Mutex<BTreeMap<PathBuf, Weak<AppContext>>>,
    #[cfg(test)]
    test_storage: tempfile::TempDir,
}

impl App {
    pub(crate) fn set_subc_connection_file(&self, path: PathBuf) {
        *self.subc_connection_file.write() = Some(path);
    }
    pub(crate) fn subc_connection_file(&self) -> Option<PathBuf> {
        self.subc_connection_file.read().clone()
    }
    pub fn new(provider_factory: LanguageProviderFactory) -> Self {
        Self {
            db: parking_lot::Mutex::new(None),
            subc_connection_file: parking_lot::RwLock::new(None),
            lifecycle_census: crate::lifecycle_census::LifecycleCensusCache::default(),
            active_watchers: AtomicUsize::new(0),
            active_actor_roots: AtomicUsize::new(0),
            open_routes: AtomicUsize::new(0),
            lsp_child_registry: crate::lsp::child_registry::LspChildRegistry::new(),
            stdout_writer: Arc::new(Mutex::new(BufWriter::new(io::stdout()))),
            provider_factory,
            memory_contexts: parking_lot::Mutex::new(BTreeMap::new()),
            #[cfg(test)]
            test_storage: tempfile::Builder::new()
                .prefix("aft-test-context-")
                .tempdir()
                .expect("private test storage"),
        }
    }

    /// Create the shared process `App` handle required by the actor split.
    pub fn shared(provider_factory: LanguageProviderFactory) -> Arc<Self> {
        Arc::new(Self::new(provider_factory))
    }

    pub fn default_shared() -> Arc<Self> {
        Self::shared(default_language_provider_factory)
    }

    pub fn create_provider(&self) -> Box<dyn LanguageProvider> {
        (self.provider_factory)()
    }

    /// All context constructors and publications share this default, including
    /// helpers in other modules. The owning App keeps the directory alive.
    #[cfg(test)]
    pub(crate) fn isolate_test_config(&self, mut config: Config) -> Config {
        if config
            .storage_dir
            .as_ref()
            .is_none_or(|p| p.as_os_str().is_empty())
        {
            config.storage_dir = Some(self.test_storage.path().to_path_buf());
        }
        crate::test_storage::assert_context(&config);
        config
    }

    pub fn lsp_child_registry(&self) -> crate::lsp::child_registry::LspChildRegistry {
        self.lsp_child_registry.clone()
    }

    pub(crate) fn publish_lifecycle_census(
        &self,
        snapshot: crate::lifecycle_census::LifecycleCensusSnapshot,
    ) {
        self.lifecycle_census.publish(snapshot);
    }

    pub(crate) fn lifecycle_census_snapshot(
        &self,
    ) -> crate::lifecycle_census::LifecycleCensusSnapshot {
        self.lifecycle_census.snapshot()
    }

    pub(crate) fn lifecycle_lsp_applicability_for_root(
        &self,
        root: &str,
    ) -> Option<crate::lsp::manager::ApplicabilityMetrics> {
        self.lifecycle_census.lsp_applicability_for_root(root)
    }

    pub fn stdout_writer(&self) -> SharedStdoutWriter {
        Arc::clone(&self.stdout_writer)
    }

    pub(crate) fn register_memory_context(&self, root: PathBuf, ctx: &Arc<AppContext>) {
        let mut contexts = self.memory_contexts.lock();
        contexts.retain(|_, context| context.strong_count() > 0);
        contexts.insert(root, Arc::downgrade(ctx));
    }

    pub(crate) fn unregister_memory_context(&self, root: &Path, ctx: &Arc<AppContext>) {
        let mut contexts = self.memory_contexts.lock();
        let removes_current = contexts
            .get(root)
            .and_then(Weak::upgrade)
            .is_some_and(|registered| Arc::ptr_eq(&registered, ctx));
        if removes_current {
            contexts.remove(root);
        }
    }

    /// Snapshot process roots without waiting behind actor registration. A busy
    /// registry is surfaced as a named status gap by the memory snapshot.
    pub(crate) fn try_memory_contexts(&self) -> Option<Vec<(PathBuf, Arc<AppContext>)>> {
        let contexts = self.memory_contexts.try_lock()?;
        Some(
            contexts
                .iter()
                .filter_map(|(root, context)| {
                    context.upgrade().map(|context| (root.clone(), context))
                })
                .collect(),
        )
    }

    /// Return the process-shared database handle, opening it only when the
    /// requested path is not already resident. The connection mutex serializes
    /// transactions from all roots; callers never hold the App lock while using
    /// the returned connection.
    pub fn open_db(
        &self,
        path: &Path,
    ) -> Result<Arc<Mutex<TrackedConnection>>, crate::db::OpenError> {
        self.open_db_with_mode(path, crate::db::OpenMode::Deferred)
    }

    pub(crate) fn open_db_with_mode(
        &self,
        path: &Path,
        mode: crate::db::OpenMode,
    ) -> Result<Arc<Mutex<TrackedConnection>>, crate::db::OpenError> {
        let key = database_path_key(path);
        let mut slot = lock_database_slot(&self.db, mode)?;
        if let Some((existing_path, conn)) = slot.as_ref() {
            if existing_path == &key {
                return Ok(Arc::clone(conn));
            }
        }

        let created = !path.exists();
        let conn = Arc::new(Mutex::new(crate::db::open_with_mode(path, mode)?));
        *slot = Some((key, Arc::clone(&conn)));
        drop(slot);
        if created {
            // The startup catalog cannot declare a database that does not
            // exist yet, so on a fresh machine it would stay undeclared until
            // the next restart. Describe the storage root again now that it
            // has one.
            if let Some(storage_root) = path.parent() {
                crate::engram_catalog::refresh_catalog(storage_root);
            }
        }
        Ok(conn)
    }

    /// Peek through the resident handle if this process already owns the file.
    /// Opening and closing another descriptor can release SQLite's process-wide
    /// locks. Hold the App slot through the peek so another root cannot open
    /// the file between checking for a handle and opening a read-only one.
    pub(crate) fn database_schema_version(
        &self,
        path: &Path,
        mode: crate::db::OpenMode,
    ) -> Result<(Option<u32>, bool), crate::db::OpenError> {
        let key = database_path_key(path);
        let slot = lock_database_slot(&self.db, mode)?;
        if let Some((existing_path, shared)) = slot.as_ref() {
            if existing_path == &key {
                let shared = Arc::clone(shared);
                drop(slot);
                let mut conn = if mode == crate::db::OpenMode::SingleAttempt {
                    shared.try_lock().map_err(|_| database_connection_busy())?
                } else {
                    shared
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                };
                return crate::db::schema_version_without_wait(&mut conn)
                    .map(|version| (Some(version), true))
                    .map_err(Into::into);
            }
        }
        Ok((crate::db::peek_schema_version(path), false))
    }

    pub fn set_db(&self, conn: Arc<Mutex<TrackedConnection>>) {
        *self.db.lock() = Some((PathBuf::new(), conn));
    }

    pub fn clear_db(&self) {
        *self.db.lock() = None;
    }

    /// Clear the shared handle only when it still refers to `path`. A failed
    /// reconfigure for one root must not tear down a database used by another
    /// root.
    pub fn clear_db_for_path(&self, path: &Path) {
        let key = database_path_key(path);
        let mut slot = self.db.lock();
        if slot.as_ref().is_some_and(|(existing_path, _)| {
            existing_path.as_os_str().is_empty() || existing_path == &key
        }) {
            *slot = None;
        }
    }

    pub fn db(&self) -> Option<Arc<Mutex<TrackedConnection>>> {
        self.db.lock().as_ref().map(|(_, conn)| Arc::clone(conn))
    }

    /// [`Self::db`] without waiting for the handle slot: the outer `None`
    /// means another thread holds it right now. The subc frame loop's
    /// maintenance sweep uses this and skips its database chores for the tick.
    pub fn try_db(&self) -> Option<Option<Arc<Mutex<TrackedConnection>>>> {
        self.db
            .try_lock()
            .map(|slot| slot.as_ref().map(|(_, conn)| Arc::clone(conn)))
    }

    #[cfg(test)]
    pub(crate) fn with_database_slot_locked_for_test(&self, f: impl FnOnce()) {
        let _slot = self.db.lock();
        f();
    }

    /// The resident handle only when it was opened for `path`. Callers that must
    /// not open a second connection to a file this process already holds use
    /// this to tell "holding it" from "holding some other database".
    pub fn db_for_path(&self, path: &Path) -> Option<Arc<Mutex<TrackedConnection>>> {
        let key = database_path_key(path);
        self.db
            .lock()
            .as_ref()
            .filter(|(existing_path, _)| existing_path == &key)
            .map(|(_, conn)| Arc::clone(conn))
    }

    pub(crate) fn watcher_started(&self) {
        self.active_watchers.fetch_add(1, Ordering::SeqCst);
    }

    #[allow(deprecated)] // fetch_update became try_update in Rust 1.99; the MSRV (1.92) lacks try_update
    pub(crate) fn watcher_stopped(&self) {
        self.active_watchers
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
                Some(count.saturating_sub(1))
            })
            .ok();
    }

    /// Number of live watcher filter runtimes registered by this process.
    /// A runtime remains counted until its OS watcher thread has actually exited.
    pub fn watcher_count(&self) -> usize {
        self.active_watchers.load(Ordering::SeqCst)
    }

    pub(crate) fn actor_root_registered(&self) {
        self.active_actor_roots.fetch_add(1, Ordering::SeqCst);
    }

    #[allow(deprecated)] // fetch_update became try_update in Rust 1.99; the MSRV (1.92) lacks try_update
    pub(crate) fn actor_root_unregistered(&self) {
        self.active_actor_roots
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
                Some(count.saturating_sub(1))
            })
            .ok();
    }

    pub fn actor_root_count(&self) -> usize {
        self.active_actor_roots.load(Ordering::SeqCst)
    }

    pub(crate) fn set_open_route_count(&self, count: usize) {
        self.open_routes.store(count, Ordering::SeqCst);
    }

    pub fn open_route_count(&self) -> usize {
        self.open_routes.load(Ordering::SeqCst)
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new(default_language_provider_factory)
    }
}

const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    fn assert_send<T: Send>() {}

    assert_send_sync::<App>();
    assert_send_sync::<AppContext>();
    assert_send::<crate::lsp::manager::LspManager>();
    assert_send::<crate::semantic_index::EmbeddingModel>();
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GitEntryKind {
    Missing,
    File,
    Directory,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct GitEntrySignature {
    kind: GitEntryKind,
    modified: Option<SystemTime>,
    /// A `.git` entry replaced within one tick of the filesystem clock keeps
    /// its kind and modification time; only its identity shows that it is a
    /// different entry.
    identity: crate::file_identity::FileIdentity,
}

#[derive(Clone, Debug)]
struct WorktreeBridgeCacheEntry {
    git_entry: GitEntrySignature,
    is_worktree_bridge: bool,
    git_common_dir: Option<PathBuf>,
}

struct ReadonlyArtifactCacheKey {
    record: Option<crate::root_identity::RecordStamp>,
    key: String,
}

fn git_entry_signature(project_root: &Path) -> GitEntrySignature {
    match crate::file_identity::symlink_metadata(&project_root.join(".git")) {
        Ok((metadata, identity)) => GitEntrySignature {
            kind: if metadata.file_type().is_file() {
                GitEntryKind::File
            } else if metadata.file_type().is_dir() {
                GitEntryKind::Directory
            } else {
                GitEntryKind::Other
            },
            modified: metadata.modified().ok(),
            identity,
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => GitEntrySignature {
            kind: GitEntryKind::Missing,
            modified: None,
            identity: crate::file_identity::FileIdentity::Unavailable,
        },
        Err(_) => GitEntrySignature {
            kind: GitEntryKind::Other,
            modified: None,
            identity: crate::file_identity::FileIdentity::Unavailable,
        },
    }
}

struct WatcherRuntimeIdentity {
    root: PathBuf,
    gitignore_generation: u64,
    #[cfg(test)]
    thread_id: Option<std::thread::ThreadId>,
}

/// Shared application context threaded through all command handlers.
///
/// Holds the language provider, backup/checkpoint stores, and configuration.
/// Constructed once at startup and passed by
/// reference to `dispatch`.
///
/// Write-rarely stores use `parking_lot::Mutex` for interior mutability so this
/// context can become thread-safe while preserving the current single-request
/// dispatch behavior. `config` is a thread-safe owned snapshot so future
/// read-only dispatch can hold configuration across other work without holding
/// a lock guard.
pub struct AppContext {
    app: Arc<App>,
    provider: Box<dyn LanguageProvider>,
    backup: parking_lot::Mutex<BackupStore>,
    checkpoint: parking_lot::Mutex<CheckpointStore>,
    /// Lets configure select the checkpoint store's durable namespace without
    /// taking `checkpoint`, which a command can hold while it waits on the
    /// cross-process checkpoint file lock.
    checkpoint_namespace_request: crate::checkpoint::CheckpointNamespaceRequest,
    config: RwLock<Arc<Config>>,
    config_publication: parking_lot::Mutex<()>,
    config_pin_identity: usize,
    /// State for applying config file edits while the root stays bound: the
    /// config file texts the last configure applied, the pending-reload flag
    /// and the root's own config file watches.
    config_live: crate::config_live::ConfigLiveState,
    /// The last successful connect request (kept for cache recovery), when it
    /// was last re-run, and the connect generation it belongs to.
    cache_recovery: parking_lot::Mutex<Option<(crate::protocol::RawRequest, Option<Instant>, u64)>>,
    /// Counts connects (configure requests other than a cache recovery). A
    /// deferred cache recovery carries the generation its request belongs to
    /// and is dropped if a connect started since, so an old connect request
    /// is never re-run over a newer connection's settings.
    connect_generation: std::sync::atomic::AtomicU64,
    // Rebinds can leave workers on old paths; keep each distinct root locked
    // until this context and its in-flight snapshots have been retired.
    cache_leases: parking_lot::Mutex<Vec<Arc<crate::root_cache::CacheLease>>>,
    /// Last tool/request activity for this root. Standalone idle LSP reclaim
    /// keys off this stamp; the subc reaper uses its own per-root `last_touched`.
    last_request_at: parking_lot::Mutex<Instant>,
    /// Per-root-actor memo for containment checks. The key is the configured
    /// root's exact `PathBuf` spelling, so reconfiguration never reuses a
    /// canonical root selected for another configured value.
    path_restriction_root_memo: parking_lot::Mutex<Option<PathRestrictionRootMemo>>,
    /// Confirmed launch materializations shared by this root's routes. Missing
    /// params and failed restores are not cached, so later calls can retry.
    launch_grants_materialized: parking_lot::Mutex<HashSet<crate::db::access_grants::ScopeKey>>,
    pub(crate) launch_grant_state: Arc<launch_grants::LaunchGrantState>,
    #[cfg(test)]
    launch_restore_attempts: AtomicUsize,
    #[cfg(test)]
    path_restriction_root_canonicalizations: AtomicUsize,
    force_restrict_requests: parking_lot::Mutex<BTreeMap<String, usize>>,
    progress_requests: parking_lot::Mutex<HashSet<String>>,
    pub harness: parking_lot::Mutex<Option<Harness>>,
    canonical_cache_root: parking_lot::Mutex<Option<PathBuf>>,
    is_worktree_bridge: parking_lot::Mutex<bool>,
    git_common_dir: parking_lot::Mutex<Option<PathBuf>>,
    /// The main checkout of this linked worktree, in the policy's spelling,
    /// verified and frozen when the cache role is set (see
    /// [`verified_linked_main_root`]).
    linked_main_root: parking_lot::Mutex<Option<PathBuf>>,
    shared_artifacts_read_only: AtomicBool,
    /// Standalone NDJSON requests may borrow a finite CLI snapshot after the
    /// writer has exited; daemon-bound routes keep their live freshness owner.
    daemonless_query_mode: AtomicBool,
    /// The search reranker backend, rebuilt off the search path whenever the
    /// published config or the daemon mode changes.
    rerank_slot: crate::search_state::rerank::slot::BackendSlot,
    callgraph_writer: AtomicBool,
    inspect_writer: AtomicBool,
    /// Reasons (if any) why heavy AFT subsystems were auto-disabled for the
    /// current project root. Populated by `handle_configure` based on the
    /// canonical project root. Each reason is a stable machine-readable string
    /// (e.g. `"home_root"`, `"watcher_unavailable"`) so the plugin can render
    /// distinct degraded-mode UI states without re-deriving the reason locally.
    /// Empty when the project is healthy / full-featured.
    degraded_reasons: parking_lot::Mutex<Vec<String>>,
    /// Configure-time gate for project-wide scans, builds, and watcher-driven
    /// refreshes that would otherwise walk the whole root. `handle_configure`
    /// closes it for degraded home roots and every heavy-work entry point reads
    /// the same atomic so the decision cannot drift after configure returns.
    heavy_root_work_allowed: Arc<AtomicBool>,
    /// Standing roots retain artifacts across idle session reaping, but they
    /// remain subject to every verification and publication fence.
    standing_artifact_exempt: AtomicBool,
    cold_build_limiter: RwLock<Arc<crate::cold_build_limiter::ColdBuildLimiter>>,
    /// This root's checkout in the data layer that replaces the views and
    /// legacy stores; only with `AFT_DATA_LAYER=1` (see
    /// `data_layer::runtime`).
    data_checkout: parking_lot::Mutex<Option<Arc<crate::data_layer::runtime::DataCheckout>>>,
    /// Set when `data_checkout` was handed to this context
    /// ([`Self::pin_data_checkout`]) rather than bound by it: it is then kept
    /// whatever this context's own root and index switches say.
    data_checkout_pinned: AtomicBool,
    /// The configure generation a standalone AFT last bound the data
    /// layer's checkout after (see
    /// [`crate::runtime_drain::follow_configure_with_data_layer`]).
    data_layer_followed_configure: AtomicU64,
    /// Why the last attempt to bind `data_checkout` failed, until one
    /// succeeds; status and health report it. Kept typed, so a store file a
    /// newer build wrote is reported as that refusal.
    data_bind_failure: parking_lot::Mutex<Option<crate::data_layer::checkout::DataError>>,
    /// When this root is a parent folder of repositories, its children's
    /// checkouts in the data layer; empty otherwise (see
    /// `data_layer::parent`).
    data_children: crate::data_layer::parent::ChildCheckouts,
    /// Repositories outside this root that `aft_search` was asked about,
    /// each answered from its own checkout in the data layer, by root (see
    /// `commands::data_search::ExternalSearch`). Released when idle.
    data_external: parking_lot::Mutex<
        std::collections::HashMap<PathBuf, Arc<crate::parent_search::external::ExternalSearch>>,
    >,
    callgraph_executor: parking_lot::Mutex<Option<crate::executor::RootMaintenanceExecutor>>,
    /// Checkout view load failures of this project (see [`ViewLoadHealth`]).
    /// Read by the health snapshot; written by every query that loads a view.
    view_load_health: parking_lot::Mutex<ViewLoadHealth>,
    search_exact_memo: Arc<crate::search_state::memo::ExactMemoStore>,
    symbol_cache: SharedSymbolCache,
    inspect_manager: Arc<InspectManager>,
    tier2_refresh_scheduler: parking_lot::Mutex<Tier2RefreshScheduler>,
    pending_tier2_paths: parking_lot::Mutex<BTreeSet<PathBuf>>,
    semantic_index: RwLock<Option<SemanticIndex>>,
    semantic_index_rx: parking_lot::Mutex<Option<crossbeam_channel::Receiver<SemanticIndexEvent>>>,
    semantic_index_rx_generation: AtomicU64,
    semantic_index_rx_epoch: AtomicU64,
    semantic_index_rx_terminal_epoch: Arc<AtomicU64>,
    /// Size of a finished semantic-index load waiting in `semantic_index_rx`.
    semantic_pending_install: Arc<PendingInstallSlot>,
    semantic_persist_epoch: crate::root_cache::ArtifactPublishEpoch,
    semantic_index_status: Arc<RwLock<SemanticIndexStatus>>,
    /// Present only while a cold semantic build is running. Its counters are
    /// read by status and health without taking the worker's batch-loop locks.
    semantic_build_progress: RwLock<Option<SemanticBuildProgress>>,
    /// Advances when the inputs that determine a semantic corpus build change.
    /// Unrelated configure changes adopt the existing worker instead.
    semantic_build_epoch: Arc<AtomicU64>,
    /// True while this context has a cold semantic seed scheduled or actively
    /// collecting/embedding/persisting the full project corpus. The semantic
    /// worker clears it as soon as it proves the cached/incremental path is in use.
    semantic_cold_seed_active: Arc<AtomicBool>,
    /// Monotonic generation that prevents a superseded semantic worker from
    /// reopening the cold-seed gate after a later configure has reset it.
    semantic_cold_seed_generation: Arc<AtomicU64>,
    semantic_fingerprint_generation: Arc<AtomicU64>,
    pending_semantic_index_paths: Arc<parking_lot::Mutex<BTreeSet<PathBuf>>>,
    pending_semantic_corpus_refresh: parking_lot::Mutex<bool>,
    semantic_refresh_tx:
        Arc<parking_lot::Mutex<Option<crossbeam_channel::Sender<SemanticRefreshRequest>>>>,
    semantic_refresh_event_rx:
        parking_lot::Mutex<Option<crossbeam_channel::Receiver<SemanticRefreshEvent>>>,
    semantic_refresh_generation: AtomicU64,
    semantic_refresh_epoch: AtomicU64,
    semantic_refresh_build_epoch: AtomicU64,
    semantic_refresh_worker: parking_lot::Mutex<Option<SemanticRefreshWorkerSlot>>,
    semantic_worker_bytes: Arc<AtomicU64>,
    semantic_refresh_retry_attempts: parking_lot::Mutex<BTreeMap<PathBuf, usize>>,
    semantic_refresh_circuit: Arc<SemanticRefreshCircuit>,
    semantic_embedding_model:
        Arc<parking_lot::Mutex<Option<crate::semantic_index::EmbeddingModel>>>,
    watcher_runtime_lock: parking_lot::Mutex<()>,
    watcher: parking_lot::Mutex<Option<RecommendedWatcher>>,
    watcher_rx: parking_lot::Mutex<Option<crossbeam_channel::Receiver<WatcherDispatchEvent>>>,
    watcher_drain_slice: parking_lot::Mutex<Option<WatcherDrainSliceState>>,
    deleted_root_cleanup: parking_lot::Mutex<Option<DeletedRootCleanup>>,
    watcher_drains_in_flight: AtomicUsize,
    /// Advances when a watcher drain starts and when it ends.
    watcher_drain_activity: AtomicU64,
    watcher_thread: parking_lot::Mutex<Option<WatcherThreadHandle>>,
    watcher_runtime_identity: parking_lot::Mutex<Option<WatcherRuntimeIdentity>>,
    watcher_counters: RwLock<Arc<WatcherCounters>>,
    lsp_manager: Arc<parking_lot::Mutex<LspManager>>,
    /// The LSP manager's "a worker used this root" flag, readable without the
    /// manager lock (see [`AppContext::disable_rust_cargo_for_worker`]).
    lsp_rust_worker_policy: Arc<std::sync::atomic::AtomicBool>,
    /// Advances with every configuration publication; a background
    /// language-server applicability walk stores its result only while it
    /// still matches (see `refresh_project_applicability`).
    project_applicability_epoch: Arc<AtomicU64>,
    /// The highest `project_applicability_epoch` value whose applicability
    /// walk has finished, however it ended. When it has caught up with
    /// `project_applicability_epoch`, no walk for the current configuration
    /// is still running.
    project_applicability_progress: Arc<ProjectApplicabilityProgress>,
    /// The last walk of this root's semantic index, trigram index and symbol
    /// cache that the health rollup made for its memory estimate. A later
    /// rollup reuses it while those indexes' `memory_shape` counters are
    /// unchanged (see `memory_root_snapshot_and_rollup`).
    health_memory_walk: parking_lot::Mutex<HealthMemoryWalks>,
    /// At most one diagnostics collector may outlive its caller's budget.
    /// Later edits queue their latest disk contents instead of growing a pool
    /// of threads waiting on a slow handshake or the same manager mutex.
    lsp_post_edit_worker_running: Arc<AtomicBool>,
    /// Watcher changes waiting for the LSP manager lock. `Some` while the one
    /// helper thread that forwards them exists; drains that find the lock
    /// busy merge into it instead of starting threads of their own (see
    /// [`Self::lsp_forward_or_queue_watcher_changes`]).
    lsp_watcher_forward_slot: crate::lsp::manager::WatcherForwardSlot,
    /// How many of those helper threads were started, for tests.
    lsp_watcher_forward_helpers_spawned: AtomicUsize,
    /// Held by tests to keep that helper thread from forwarding anything
    /// (see [`Self::pause_lsp_watcher_forwarding_for_test`]); never held
    /// otherwise.
    lsp_watcher_forward_pause: Arc<parking_lot::Mutex<()>>,
    #[cfg(test)]
    lsp_classified_watcher_forwards: parking_lot::Mutex<Option<Vec<Vec<PathBuf>>>>,
    /// Document and configuration-file changes waiting for the LSP manager
    /// lock (see [`crate::lsp::pending_changes`]); a busy lock queues them
    /// instead of dropping them.
    lsp_pending_change_slot: crate::lsp::pending_changes::PendingLspChangeSlot,
    configure_generation: Arc<AtomicU64>,
    /// Advances only when the warm configuration changes, not on route
    /// teardown. Already-admitted workers use it to decide whether their disk
    /// artifact is still configuration-compatible after becoming unbound.
    configure_content_generation: Arc<AtomicU64>,
    /// Set only by the daemon route lifecycle. Standalone contexts remain bound.
    /// Deferred maintenance uses the same gate for the state check and admission.
    subc_lifecycle: SubcLifecycleAdmission,
    configure_warm_state: parking_lot::Mutex<ConfigureWarmState>,
    configure_phase_timing: parking_lot::Mutex<ConfigurePhaseTiming>,
    /// The configure-tail stage running on this root and when it started, or
    /// `None` between units. Read by the pending-bind diagnostics.
    configure_tail_stage: parking_lot::Mutex<Option<(&'static str, Instant)>>,
    configured_session_roots: parking_lot::Mutex<BTreeSet<(PathBuf, String)>>,
    hashline_bindings: crate::hashline::integration::BindingRegistry,
    configure_maintenance_jobs: parking_lot::Mutex<VecDeque<ConfigureMaintenanceJob>>,
    // 0: never configured, 1: opening, 2: ready, 3: failed,
    // 4: busy not yet reported, 5: busy reported (next call may retry). Dispatch must not
    // run persistence-dependent tools while the root's database is opening.
    database_runtime_state: AtomicU8,
    database_runtime_changed: tokio::sync::Notify,
    database_runtime_failure: parking_lot::Mutex<DatabaseRuntimeFailure>,
    /// Whether each scoped worker (identified by its remote-exec policy key)
    /// saved `confine: "root"`, as far as this context has read or saved it,
    /// so tool calls can skip the aft.db read and its lock. See
    /// [`PolicyConfinementMemo`].
    policy_confinement: parking_lot::Mutex<PolicyConfinementMemo>,
    standalone_approval_route: Arc<()>,
    standalone_plugin_host: AtomicBool,
    #[cfg(test)]
    database_open_attempts: AtomicU64,
    /// The pending aft.db open request, the configure epoch it belongs to, and
    /// whether an open is running. See `crate::database_open`.
    database_open: parking_lot::Mutex<crate::database_open::DatabaseOpenSlot>,
    /// Signalled whenever an open for this root stops running.
    database_open_idle: parking_lot::Condvar,
    /// Configure-tail work that stepped aside mid-drain so a queued
    /// interactive writer (usually a route bind) could take the actor. The
    /// next tail drain resumes it before anything newly enqueued.
    parked_configure_tail:
        parking_lot::Mutex<Option<crate::commands::configure::ConfigureMaintenanceState>>,
    /// The background identity retry armed when aft.db was busy at this
    /// root's bind. See [`crate::provisional_identity_retry`].
    provisional_identity_retry: crate::provisional_identity_retry::RetrySlot,
    artifact_cache_keys: parking_lot::Mutex<BTreeMap<PathBuf, String>>,
    /// Only successful configure binds may populate this memo. Build-derived
    /// keys remain observable by status but cannot skip identity verification.
    /// Entries are keyed by (storage root, project path): each storage root
    /// keeps its own saved identity record, so a reconfigure that moves the
    /// cache location must read the new root's record instead of reusing the
    /// key another storage root chose for the same folder.
    configure_artifact_cache_keys: parking_lot::Mutex<BTreeMap<(PathBuf, PathBuf), String>>,
    readonly_artifact_cache_keys:
        parking_lot::Mutex<BTreeMap<(PathBuf, PathBuf), ReadonlyArtifactCacheKey>>,
    artifact_cache_key_derivations: AtomicU64,
    /// Successful git worktree probes, keyed by canonical root and guarded by
    /// the root's `.git` entry shape and modification time.
    worktree_bridge_cache: parking_lot::Mutex<BTreeMap<PathBuf, WorktreeBridgeCacheEntry>>,
    #[cfg(test)]
    worktree_bridge_probe_spawns: AtomicU64,
    #[cfg(test)]
    force_worktree_bridge_reprobe: AtomicBool,
    configure_warnings_tx: crossbeam_channel::Sender<(u64, ConfigureWarningsFrame)>,
    configure_warnings_rx: crossbeam_channel::Receiver<(u64, ConfigureWarningsFrame)>,
    /// Per-context push sender slot. Status and background-bash emitters share
    /// this Arc so a sender installed after construction is observed at emit time.
    progress_sender: SharedProgressSender,
    status_emitter: StatusEmitter,
    /// Present only for daemon-bound actors. Standalone NDJSON contexts never
    /// acquire a status-holder transport and therefore keep the solo bar path.
    fleet_status_client: RwLock<Option<crate::fleet_status::FleetStatusClient>>,
    /// Temporary state used to avoid repeatedly emitting the legacy status-bar
    /// response fields. It is no longer needed once those fields are removed.
    status_bar_last_emitted: StatusBarEmission,
    /// The omission-preserving source of truth. Legacy projections never enter
    /// this cache, so a cache hit cannot recreate an absent category as zero.
    status_bar_cached: RwLock<StatusBarCache>,
    /// Authoritative diagnostics observations are retained per session and
    /// producer. Only explicit observation sources may mutate this state.
    alert_state: parking_lot::Mutex<AlertDeltaState>,
    repeat_breaker: crate::repeat_state::RepeatBreaker,
    compression_aggregates: Arc<crate::db::compression_events::CompressionAggregateCache>,
    bash_background: BgTaskRegistry,
    output_files: Arc<crate::output_store::OutputFiles>,
    #[cfg(unix)]
    escalation_grants: parking_lot::Mutex<crate::sandbox_spawn::EscalationGrantStore>,
    /// Thread-safe registry of TOML output filters. Lazy-built on first
    /// access; populated atomically via `RwLock`. Shared between command
    /// handlers (which use it through `filter_registry()` -> read guard) and
    /// the `BgTaskRegistry` watchdog thread (which uses it through
    /// `compress::compress_with_registry`). Reloaded when configure changes
    /// the project root or storage_dir; see [`AppContext::reset_filter_registry`].
    filter_registry: crate::compress::SharedFilterRegistry,
    filter_registry_rebuild_count: AtomicU64,
    /// Set to true once the filter_registry has been populated. Avoids
    /// double-loading on hot paths without holding a write lock.
    filter_registry_loaded: std::sync::atomic::AtomicBool,
    /// Set when idle eviction cleared the shared registry slot, until a
    /// background bash spawn has loaded it again (see
    /// [`AppContext::reload_released_filter_registry`]).
    filter_registry_released: std::sync::atomic::AtomicBool,
    /// Set when an idle release kept the filters because the root still held
    /// background task records. The idle reaper retries the release once the
    /// last record is gone (see [`AppContext::retry_deferred_idle_release`]).
    filter_release_deferred: AtomicBool,
    /// Live `experimental.bash.compress` flag, kept in sync with `config`
    /// from the configure handler. Exposed via [`AppContext::bash_compress_flag`]
    /// so the BgTaskRegistry's watchdog-thread compressor can read it without
    /// holding the config refcell.
    bash_compress_flag: Arc<std::sync::atomic::AtomicBool>,
    /// Project gitignore matcher, rebuilt by [`AppContext::rebuild_gitignore`]
    /// whenever `project_root` changes or a watcher event reports a
    /// `.gitignore` write. Used by the watcher event filter to decide which
    /// path-changes are interesting to AFT's caches. `None` when no project
    /// root is configured or when the project has no gitignore files; in that
    /// case the watcher falls back to a small hardcoded infra-directory skip.
    gitignore: SharedGitignore,
    gitignore_generation: Arc<AtomicU64>,
    gitignore_inputs: parking_lot::Mutex<Option<gitignore_state::IgnoreInputSnapshot>>,
    gitignore_matcher_rebuilds: AtomicU64,
    /// Last-known TODO count for the agent status bar, refreshed off the hot
    /// path (on `aft_inspect` reads and background TODO scans).
    /// Errors/warnings are read live and not stored here.
    status_bar_todos: RwLock<StatusBarTodos>,
    /// Persistent TypeScript-project membership cache for the status-bar E/W
    /// count. The bar reads E/W live on every tool result, so resolving the
    /// nearest tsconfig (read + parse + glob-compile) per drain is too costly;
    /// this memoizes per tsconfig dir. Invalidated wholesale on any
    /// tsconfig-like watcher event and on `configure`. Owned here (not in
    /// `DiagnosticsStore`, which stays raw policy-free) per the v0.35 council.
    tsconfig_membership:
        parking_lot::Mutex<crate::lsp::tsconfig_membership::TsconfigMembershipCache>,
}

/// RAII guard for a server-owned request-scoped path-restriction override.
///
/// Guards are refcounted by request id so duplicated ids over-restrict until the
/// last worker exits, rather than letting one completion disable another
/// in-flight request's containment.
pub struct ForceRestrictGuard<'a> {
    ctx: &'a AppContext,
    req_id: String,
}

impl Drop for ForceRestrictGuard<'_> {
    fn drop(&mut self) {
        self.ctx.release_force_restrict(&self.req_id);
    }
}

impl Drop for AppContext {
    fn drop(&mut self) {
        self.subc_lifecycle
            .mark_unbound(self.configure_generation.as_ref());
        if let Some(runtime) = self.watcher_thread.get_mut().take() {
            let root = self
                .canonical_cache_root
                .get_mut()
                .clone()
                .or_else(|| {
                    self.config
                        .get_mut()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .project_root
                        .clone()
                })
                .unwrap_or_else(|| PathBuf::from("<unconfigured>"));
            Self::spawn_watcher_shutdown(Arc::clone(&self.app), root, runtime);
        }
    }
}

#[cfg(test)]
struct CallgraphBuildStartGate {
    reached: crossbeam_channel::Sender<()>,
    release: crossbeam_channel::Receiver<()>,
}

#[cfg(test)]
static CALLGRAPH_BUILD_START_GATE: std::sync::OnceLock<
    parking_lot::Mutex<BTreeMap<PathBuf, CallgraphBuildStartGate>>,
> = std::sync::OnceLock::new();

#[cfg(test)]
fn install_callgraph_build_start_gate(
    root: PathBuf,
) -> (
    crossbeam_channel::Receiver<()>,
    crossbeam_channel::Sender<()>,
) {
    let (reached_tx, reached_rx) = crossbeam_channel::bounded(1);
    let (release_tx, release_rx) = crossbeam_channel::bounded(1);
    let previous = CALLGRAPH_BUILD_START_GATE
        .get_or_init(|| parking_lot::Mutex::new(BTreeMap::new()))
        .lock()
        .insert(
            root,
            CallgraphBuildStartGate {
                reached: reached_tx,
                release: release_rx,
            },
        );
    assert!(
        previous.is_none(),
        "callgraph build-start gate already installed for root"
    );
    (reached_rx, release_tx)
}

/// `try_read` that uses a poisoned lock's data, as the blocking paths in this
/// module do, and returns `None` only when another thread holds the lock.
fn try_read_unpoisoned<T>(lock: &RwLock<T>) -> Option<std::sync::RwLockReadGuard<'_, T>> {
    match lock.try_read() {
        Ok(guard) => Some(guard),
        Err(std::sync::TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner()),
        Err(std::sync::TryLockError::WouldBlock) => None,
    }
}

/// The `try_write` counterpart of [`try_read_unpoisoned`].
fn try_write_unpoisoned<T>(lock: &RwLock<T>) -> Option<std::sync::RwLockWriteGuard<'_, T>> {
    match lock.try_write() {
        Ok(guard) => Some(guard),
        Err(std::sync::TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner()),
        Err(std::sync::TryLockError::WouldBlock) => None,
    }
}

/// The locks behind the subc maintenance probes (see
/// [`AppContext::watcher_drain_has_work`] and its siblings), one per field a
/// probe reads. Tests hold each in turn to prove no probe waits for it.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MaintenanceProbeLock {
    WatcherReceiver,
    WatcherDrainSlice,
    Tier2RefreshScheduler,
    LspManager,
    ConfigureMaintenanceJobs,
    ParkedConfigureTail,
    SemanticIndexReceiver,
    SemanticRefreshEvents,
    SemanticRefreshWorker,
}

#[cfg(test)]
impl MaintenanceProbeLock {
    pub(crate) const ALL: [Self; 9] = [
        Self::WatcherReceiver,
        Self::WatcherDrainSlice,
        Self::Tier2RefreshScheduler,
        Self::LspManager,
        Self::ConfigureMaintenanceJobs,
        Self::ParkedConfigureTail,
        Self::SemanticIndexReceiver,
        Self::SemanticRefreshEvents,
        Self::SemanticRefreshWorker,
    ];
}

#[cfg(test)]
fn wait_on_callgraph_build_start_gate(root: &Path) {
    // Remove only the worker's root, and release the registry lock before waiting
    // so a held worker cannot prevent another root from reaching its own gate.
    let gate = CALLGRAPH_BUILD_START_GATE
        .get_or_init(|| parking_lot::Mutex::new(BTreeMap::new()))
        .lock()
        .remove(root);
    if let Some(gate) = gate {
        let _ = gate.reached.send(());
        let _ = gate.release.recv();
    }
}

// Retain the aggregate for integration harnesses compiled without cfg(test).
// In-process unit tests use the context-local counter instead: resetting an
// aggregate can erase another test's admission, and its workers can inflate it.
static CALLGRAPH_COLD_BUILD_SPAWN_COUNT: AtomicUsize = AtomicUsize::new(0);

#[doc(hidden)]
pub fn reset_callgraph_cold_build_spawn_count_for_test() {
    CALLGRAPH_COLD_BUILD_SPAWN_COUNT.store(0, Ordering::SeqCst);
}

#[doc(hidden)]
pub fn callgraph_cold_build_spawn_count_for_test() -> usize {
    CALLGRAPH_COLD_BUILD_SPAWN_COUNT.load(Ordering::SeqCst)
}

impl AppContext {
    /// Register the owning handle without making the status worker retain a root.
    pub(crate) fn bind_status_context(self: &Arc<Self>) {
        *self
            .status_emitter
            .context
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Arc::downgrade(self);
    }

    pub fn signal_status_changed(&self) {
        let context = self
            .status_emitter
            .context
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if context.strong_count() > 0 {
            self.status_emitter.signal_context(context);
        } else {
            // Stack-owned contexts are supported by the library API. Transport
            // roots register their owning Arc before serving any requests.
            if let Some(snapshot) = self.try_build_status_push_snapshot() {
                self.status_emitter().signal(snapshot);
            }
        }
    }

    pub fn new(provider: Box<dyn LanguageProvider>, config: Config) -> Self {
        Self::with_app_and_provider(App::default_shared(), provider, config)
    }

    pub fn from_app(app: Arc<App>, config: Config) -> Self {
        let provider = app.create_provider();
        Self::with_app_and_provider(app, provider, config)
    }

    pub fn with_app_and_provider(
        app: Arc<App>,
        provider: Box<dyn LanguageProvider>,
        config: Config,
    ) -> Self {
        #[cfg(test)]
        let config = app.isolate_test_config(config);
        let mut config = config;
        if (config.cache_dir.is_some()
            || config.cache_config_unknown.is_some()
            || config.cache_unavailable.is_some())
            && (config.cache_lease.is_none() || config.storage_lease.is_none())
        {
            if let Some(warning) = crate::root_cache::prepare_process_cache_root(&mut config) {
                crate::slog_warn!("{}: {}", warning.code, warning.message);
            }
        }
        let bash_compress_enabled = config.experimental_bash_compress;
        let watcher_counters = config
            .project_root
            .as_deref()
            .map(watcher_counters_for_root)
            .unwrap_or_else(|| Arc::new(WatcherCounters::default()));
        let (configure_warnings_tx, configure_warnings_rx) = crossbeam_channel::unbounded();
        let progress_sender: SharedProgressSender = Arc::new(Mutex::new(None));
        let status_emitter = StatusEmitter::new(Arc::clone(&progress_sender));
        let heavy_root_work_allowed = Arc::new(AtomicBool::new(true));
        let semantic_cold_seed_active = Arc::new(AtomicBool::new(false));
        let subc_lifecycle = SubcLifecycleAdmission::default();
        let inspect_manager = Arc::new(InspectManager::with_root_work_gates(
            Arc::clone(&heavy_root_work_allowed),
            Arc::clone(&semantic_cold_seed_active),
        ));
        inspect_manager.set_root_lifecycle(subc_lifecycle.clone());
        let symbol_cache = provider
            .as_any()
            .downcast_ref::<TreeSitterProvider>()
            .map(|provider| provider.symbol_cache())
            .unwrap_or_else(|| Arc::new(std::sync::RwLock::new(SymbolCache::new())));
        let mut lsp_manager = LspManager::new();
        lsp_manager.set_child_registry(app.lsp_child_registry());
        lsp_manager.set_search_paths(config.lsp_paths_extra.clone());
        // Apply the configured diagnostic LRU cap (default 5000, 0 = unbounded)
        // so the documented `lsp.diagnostic_cache_size` knob takes effect.
        lsp_manager.set_diagnostic_capacity(config.diagnostic_cache_size);
        let bash_background = BgTaskRegistry::new(Arc::clone(&progress_sender));
        let compression_aggregates = bash_background.compression_aggregate_cache();
        let checkpoint_store = CheckpointStore::new();
        let checkpoint_namespace_request = checkpoint_store.namespace_request();
        let cache_leases = config.cache_lease.clone().into_iter().collect();
        let context = AppContext {
            app: Arc::clone(&app),
            provider,
            backup: parking_lot::Mutex::new(BackupStore::new()),
            checkpoint: parking_lot::Mutex::new(checkpoint_store),
            checkpoint_namespace_request,
            config: RwLock::new(Arc::new(config)),
            config_publication: parking_lot::Mutex::new(()),
            config_pin_identity: next_config_pin_identity(),
            config_live: crate::config_live::ConfigLiveState::default(),
            cache_recovery: parking_lot::Mutex::new(None),
            connect_generation: std::sync::atomic::AtomicU64::new(0),
            cache_leases: parking_lot::Mutex::new(cache_leases),
            last_request_at: parking_lot::Mutex::new(Instant::now()),
            path_restriction_root_memo: parking_lot::Mutex::new(None),
            launch_grants_materialized: parking_lot::Mutex::new(HashSet::new()),
            launch_grant_state: Arc::default(),
            #[cfg(test)]
            launch_restore_attempts: AtomicUsize::new(0),
            #[cfg(test)]
            path_restriction_root_canonicalizations: AtomicUsize::new(0),
            force_restrict_requests: parking_lot::Mutex::new(BTreeMap::new()),
            progress_requests: parking_lot::Mutex::new(HashSet::new()),
            harness: parking_lot::Mutex::new(None),
            canonical_cache_root: parking_lot::Mutex::new(None),
            is_worktree_bridge: parking_lot::Mutex::new(false),
            git_common_dir: parking_lot::Mutex::new(None),
            linked_main_root: parking_lot::Mutex::new(None),
            shared_artifacts_read_only: AtomicBool::new(false),
            daemonless_query_mode: AtomicBool::new(false),
            rerank_slot: Default::default(),
            callgraph_writer: AtomicBool::new(true),
            inspect_writer: AtomicBool::new(true),
            degraded_reasons: parking_lot::Mutex::new(Vec::new()),
            heavy_root_work_allowed: Arc::clone(&heavy_root_work_allowed),
            standing_artifact_exempt: AtomicBool::new(false),
            cold_build_limiter: RwLock::new(crate::cold_build_limiter::global_limiter()),
            data_checkout: parking_lot::Mutex::new(None),
            data_checkout_pinned: AtomicBool::new(false),
            data_layer_followed_configure: AtomicU64::new(0),
            data_bind_failure: parking_lot::Mutex::new(None),
            data_children: Default::default(),
            data_external: parking_lot::Mutex::new(std::collections::HashMap::new()),
            callgraph_executor: parking_lot::Mutex::new(None),
            view_load_health: parking_lot::Mutex::new(ViewLoadHealth::default()),
            search_exact_memo: Arc::new(
                crate::search_state::memo::ExactMemoStore::new(),
            ),
            symbol_cache,
            inspect_manager,
            tier2_refresh_scheduler: parking_lot::Mutex::new(Tier2RefreshScheduler::new()),
            pending_tier2_paths: parking_lot::Mutex::new(BTreeSet::new()),
            semantic_index: RwLock::new(None),
            semantic_index_rx: parking_lot::Mutex::new(None),
            semantic_index_rx_generation: AtomicU64::new(0),
            semantic_index_rx_epoch: AtomicU64::new(0),
            semantic_index_rx_terminal_epoch: Arc::new(AtomicU64::new(0)),
            semantic_pending_install: Arc::default(),
            semantic_persist_epoch: crate::root_cache::ArtifactPublishEpoch::default(),
            semantic_index_status: Arc::new(RwLock::new(SemanticIndexStatus::Disabled)),
            semantic_build_progress: RwLock::new(None),
            semantic_build_epoch: Arc::new(AtomicU64::new(0)),
            semantic_cold_seed_active,
            semantic_cold_seed_generation: Arc::new(AtomicU64::new(0)),
            semantic_fingerprint_generation: Arc::new(AtomicU64::new(0)),
            pending_semantic_index_paths: Arc::new(parking_lot::Mutex::new(BTreeSet::new())),
            pending_semantic_corpus_refresh: parking_lot::Mutex::new(false),
            semantic_refresh_tx: Arc::new(parking_lot::Mutex::new(None)),
            semantic_refresh_event_rx: parking_lot::Mutex::new(None),
            semantic_refresh_generation: AtomicU64::new(0),
            semantic_refresh_epoch: AtomicU64::new(0),
            semantic_refresh_build_epoch: AtomicU64::new(0),
            semantic_refresh_worker: parking_lot::Mutex::new(None),
            semantic_worker_bytes: Arc::default(),
            semantic_refresh_retry_attempts: parking_lot::Mutex::new(BTreeMap::new()),
            semantic_refresh_circuit: Arc::new(SemanticRefreshCircuit::default()),
            semantic_embedding_model: Arc::new(parking_lot::Mutex::new(None)),
            watcher_runtime_lock: parking_lot::Mutex::new(()),
            watcher: parking_lot::Mutex::new(None),
            watcher_rx: parking_lot::Mutex::new(None),
            watcher_drain_slice: parking_lot::Mutex::new(None),
            deleted_root_cleanup: parking_lot::Mutex::new(None),
            watcher_drains_in_flight: AtomicUsize::new(0),
            watcher_drain_activity: AtomicU64::new(0),
            watcher_thread: parking_lot::Mutex::new(None),
            watcher_runtime_identity: parking_lot::Mutex::new(None),
            watcher_counters: RwLock::new(watcher_counters),
            lsp_rust_worker_policy: lsp_manager.rust_worker_policy(),
            lsp_manager: Arc::new(parking_lot::Mutex::new(lsp_manager)),
            project_applicability_epoch: Arc::new(AtomicU64::new(0)),
            project_applicability_progress: Arc::default(),
            health_memory_walk: parking_lot::Mutex::new(HealthMemoryWalks::default()),
            lsp_post_edit_worker_running: Arc::new(AtomicBool::new(false)),
            lsp_watcher_forward_slot: Default::default(),
            lsp_watcher_forward_helpers_spawned: AtomicUsize::new(0),
            lsp_watcher_forward_pause: Arc::default(),
            #[cfg(test)]
            lsp_classified_watcher_forwards: Default::default(),
            lsp_pending_change_slot: Default::default(),
            configure_generation: Arc::new(AtomicU64::new(0)),
            configure_content_generation: Arc::new(AtomicU64::new(0)),
            subc_lifecycle,
            configure_warm_state: parking_lot::Mutex::new(ConfigureWarmState::default()),
            configure_phase_timing: parking_lot::Mutex::new(ConfigurePhaseTiming::default()),
            configure_tail_stage: parking_lot::Mutex::new(None),
            configured_session_roots: parking_lot::Mutex::new(BTreeSet::new()),
            hashline_bindings: crate::hashline::integration::BindingRegistry::new(),
            configure_maintenance_jobs: parking_lot::Mutex::new(VecDeque::new()),
            database_runtime_state: AtomicU8::new(0),
            database_runtime_changed: tokio::sync::Notify::new(),
            database_runtime_failure: parking_lot::Mutex::new(DatabaseRuntimeFailure::default()),
            policy_confinement: parking_lot::Mutex::new(PolicyConfinementMemo::default()),
            standalone_approval_route: Arc::new(()),
            standalone_plugin_host: AtomicBool::new(false),
            #[cfg(test)]
            database_open_attempts: AtomicU64::new(0),
            database_open: parking_lot::Mutex::new(Default::default()),
            database_open_idle: parking_lot::Condvar::new(),
            parked_configure_tail: parking_lot::Mutex::new(None),
            provisional_identity_retry: Default::default(),
            artifact_cache_keys: parking_lot::Mutex::new(BTreeMap::new()),
            configure_artifact_cache_keys: parking_lot::Mutex::new(BTreeMap::new()),
            readonly_artifact_cache_keys: parking_lot::Mutex::new(BTreeMap::new()),
            artifact_cache_key_derivations: AtomicU64::new(0),
            worktree_bridge_cache: parking_lot::Mutex::new(BTreeMap::new()),
            #[cfg(test)]
            worktree_bridge_probe_spawns: AtomicU64::new(0),
            #[cfg(test)]
            force_worktree_bridge_reprobe: AtomicBool::new(false),
            configure_warnings_tx,
            configure_warnings_rx,
            progress_sender: Arc::clone(&progress_sender),
            status_emitter,
            fleet_status_client: RwLock::new(None),
            status_bar_last_emitted: StatusBarEmission::default(),
            status_bar_cached: RwLock::new(StatusBarCache::default()),
            alert_state: parking_lot::Mutex::new(AlertDeltaState::default()),
            repeat_breaker: crate::repeat_state::RepeatBreaker::default(),
            compression_aggregates,
            bash_background,
            output_files: crate::output_store::OutputFiles::shared(),
            #[cfg(unix)]
            escalation_grants: parking_lot::Mutex::new(
                crate::sandbox_spawn::EscalationGrantStore::default(),
            ),
            filter_registry: Arc::new(std::sync::RwLock::new(
                crate::compress::toml_filter::FilterRegistry::default(),
            )),
            filter_registry_rebuild_count: AtomicU64::new(0),
            filter_registry_loaded: std::sync::atomic::AtomicBool::new(false),
            filter_registry_released: std::sync::atomic::AtomicBool::new(false),
            filter_release_deferred: AtomicBool::new(false),
            bash_compress_flag: Arc::new(std::sync::atomic::AtomicBool::new(bash_compress_enabled)),
            gitignore: Arc::new(std::sync::RwLock::new(None)),
            gitignore_generation: Arc::new(AtomicU64::new(0)),
            gitignore_inputs: parking_lot::Mutex::new(None),
            gitignore_matcher_rebuilds: AtomicU64::new(0),
            status_bar_todos: RwLock::new(StatusBarTodos::default()),
            tsconfig_membership: parking_lot::Mutex::new(
                crate::lsp::tsconfig_membership::TsconfigMembershipCache::new(),
            ),
        };
        #[cfg(test)]
        let context = context.with_test_checkpoint_store();
        context
            .lsp_manager
            .lock()
            .enable_background_document_pulls(Arc::downgrade(&context.lsp_manager));
        crate::logging::sync_storage_root(context.storage_dir());
        context
    }

    /// Explicit status reads wait for a coherent diagnostic snapshot. Publishers
    /// use the nonblocking accessor so a busy manager never clears their bar.
    pub fn status_bar_count_values(&self) -> StatusBarCountValues {
        self.status_bar_count_values_inner(true)
            .expect("a blocking status read obtains the language-server manager")
            .mask_disabled(&self.config().inspect)
    }

    /// A busy manager is not a missing diagnostic producer. Publishers must
    /// leave their previous bar untouched and retry on a later signal.
    pub(crate) fn try_status_bar_count_values(&self) -> Option<StatusBarCountValues> {
        self.status_bar_count_values_inner(false)
    }

    fn status_bar_count_values_inner(
        &self,
        wait_for_manager: bool,
    ) -> Option<StatusBarCountValues> {
        #[cfg(test)]
        STATUS_COUNT_PROBES.with(|count| count.set(count.get() + 1));
        let todos = self
            .status_bar_todos
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let tsconfig_generation = self.tsconfig_membership.lock().generation();
        let lsp = if wait_for_manager {
            self.lsp_manager.lock()
        } else {
            self.lsp_manager.try_lock()?
        };
        let diagnostics_generation = lsp.diagnostics_generation();
        let (authoritative, producer_gaps) = lsp.project_diagnostics_authority();
        let pending_reason = if lsp.has_project_applicability() {
            "producer_reports_pending"
        } else if self.project_applicability_walk_running() {
            "applicability_walk_running"
        } else {
            "applicability_unknown"
        };
        let mask_failed = |mut counts: StatusBarCountValues| {
            if !authoritative {
                counts.errors = None;
                counts.warnings = None;
            }
            (counts.diagnostics, counts.diagnostics_reason) = if !producer_gaps.is_empty() {
                (Some("producer_missing"), None)
            } else if !authoritative {
                (Some("pending"), Some(pending_reason))
            } else {
                (None, None)
            };
            counts.diagnostics_gaps = producer_gaps.clone();
            counts
        };

        {
            let cached = self
                .status_bar_cached
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if cached.valid
                && cached.diagnostics_authoritative == authoritative
                && cached.diagnostics_generation == diagnostics_generation
                && cached.todos_generation == todos.generation
                && cached.tsconfig_generation == tsconfig_generation
            {
                return Some(
                    mask_failed(
                        cached
                            .counts
                            .clone()
                            .expect("a valid status-count cache carries truthful values"),
                    )
                    .mask_disabled(&self.config().inspect),
                );
            }
        }

        let previous_authoritative = self
            .status_bar_cached
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .counts
            .as_ref()
            .map(|counts| (counts.errors, counts.warnings));
        let ((current_errors, current_warnings), provisional) =
            match self.canonical_cache_root_opt() {
                Some(root) => {
                    // The cache root is identity-domain (bare-canonical, verbatim on
                    // Windows) while diagnostics store keys are normalized; normalize a
                    // comparison-local copy or the starts_with filter drops every diagnostic.
                    let root = crate::path_identity::normalize_path(&root);
                    let mut membership = self.tsconfig_membership.lock();
                    lsp.filtered_error_warning_counts_with_provisional(|file| {
                        file.starts_with(&root) && !membership.should_skip_diagnostics(file)
                    })
                }
                None => lsp.warm_error_warning_counts_with_provisional(),
            };
        let provisional = provisional || lsp.project_analysis_progress_pending();
        let (errors, warnings) = if provisional {
            // A warming producer or report cannot prove current E/W values. Preserve a prior
            // authoritative pair when available; otherwise omit both categories.
            previous_authoritative.unwrap_or((None, None))
        } else if lsp.has_any_diagnostic_reports() {
            (Some(current_errors), Some(current_warnings))
        } else {
            (None, None)
        };
        // Cache availability-masked values: a hidden zero is not a prior
        // certificate that a later provisional report may reuse.
        let counts = mask_failed(StatusBarCountValues {
            errors,
            warnings,
            todos: todos.todos,
            disabled_categories: Vec::new(),
            diagnostics: None,
            diagnostics_reason: None,
            diagnostics_gaps: Vec::new(),
        });

        *self
            .status_bar_cached
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = StatusBarCache {
            valid: true,
            diagnostics_authoritative: authoritative,
            diagnostics_generation,
            todos_generation: todos.generation,
            tsconfig_generation,
            counts: Some(counts.clone()),
        };
        Some(counts.mask_disabled(&self.config().inspect))
    }

    /// Provides legacy numeric status-bar fields to callers that still require
    /// them. The truthful accessor above remains the source of category presence.
    pub fn status_bar_counts(&self) -> Option<StatusBarCounts> {
        self.status_bar_count_values().legacy_projection()
    }

    pub(crate) fn try_health_summary(&self) -> RootHealthSummary {
        self.try_health_summary_for_root(None)
    }

    /// [`Self::try_health_summary`] against an already-resolved cache root.
    /// The health rollup resolves each distinct root once per tick and passes
    /// it here so a many-context tick does not re-resolve the same root once
    /// per context. `None` resolves as usual for callers without a tick memo.
    pub(crate) fn try_health_summary_for_root(
        &self,
        cache_dir: Option<&Path>,
    ) -> RootHealthSummary {
        // Read lifecycle state before taking artifact locks. Worker admission takes
        // the lifecycle lock first and then installs artifact receivers, so the
        // reverse order here would deadlock a health poll against worker startup.
        let heavy_root_work_allowed = match self.try_heavy_root_work_allowed() {
            Some(allowed) => allowed,
            None => return RootHealthSummary::busy(),
        };
        let config = match self.config.try_read() {
            Ok(guard) => Arc::clone(&*guard),
            Err(_) => return RootHealthSummary::busy(),
        };
        let cache_unavailable_reason = match cache_dir {
            Some(cache_dir) => crate::root_cache::cache_unavailable_for_root(&config, cache_dir)
                .map(|reason| crate::root_cache::cache_status_code(&reason)),
            None => crate::root_cache::cache_unavailable(&config)
                .map(|reason| crate::root_cache::cache_status_code(&reason)),
        };
        let cache_unavailable = cache_unavailable_reason.is_some();
        // Answers from other subsystems are taken before any of this
        // context's guards below. The guards are held until the summary is
        // built, and the frame loop's maintenance probes read the same
        // fields: a call into another subsystem made while holding them
        // would pin those fields for as long as that subsystem is slow.
        let semantic_build_progress = match self.semantic_build_progress.try_read() {
            Ok(guard) => guard.clone(),
            Err(_) => return RootHealthSummary::busy(),
        };
        let semantic_backend = match self.try_semantic_backend_health_snapshot() {
            Some(snapshot) => snapshot,
            None => return RootHealthSummary::busy(),
        };
        // Read the inspect builder registry (the same map used to refuse inspect
        // work while a rebuild is registered). Published status-bar counts are
        // not a substitute: a complete snapshot with a rebuild still registered
        // is still building.
        let tier2_builder_busy = match self.inspect_manager.try_tier2_builder_busy() {
            Some(busy) => busy,
            None => return RootHealthSummary::busy(),
        };
        let bash = match self.bash_background.try_health_counts() {
            Some(counts) => counts,
            None => return RootHealthSummary::busy(),
        };
        // Build suspensions were read from the call-graph build breaker under
        // the old `callgraph/` store, which this release no longer builds;
        // the index store reports its own failures in `data_store`.
        let suspended_domains = Vec::new();
        // The index store's checkout planes stand in for the search index
        // and call-graph store below. Read here, ahead of the guards: it
        // takes the checkout's own locks.
        let data_store = crate::data_layer::surfaces::report(self);
        // View load failures are recorded on every query that loads a view,
        // independent of the v1 publication runtime above: checkout-routed
        // roots report them even while `views` is `None`.
        let (view_load_failures, view_load_failure_last, view_storage_reserve): (
            BTreeMap<String, u64>,
            _,
            _,
        ) = match self.view_load_health.try_lock() {
            Some(health) => (
                health
                    .by_cause
                    .iter()
                    .map(|((plane, cause), count)| (format!("{plane}:{cause}"), *count))
                    .collect(),
                health.last.clone(),
                health.disk_reserve.clone(),
            ),
            None => return RootHealthSummary::busy(),
        };
        // A parent folder's worker reads its children's views itself; their
        // failures are recorded under the parent root and reported here too.

        let semantic_status = match self.semantic_index_status.try_read() {
            Ok(guard) => guard,
            Err(_) => return RootHealthSummary::busy(),
        };

        // The index store's report below replaces this whenever a checkout
        // is probed.
        let search_index_status = if cache_unavailable {
            crate::root_cache::CACHE_DIR_UNAVAILABLE
        } else if config.indexes.trigram {
            "building"
        } else {
            "disabled"
        };
        let mut semantic_index = if !semantic_backend.available {
            SemanticHealthComponentSnapshot {
                status: if semantic_backend.paused_until_ms.is_some() {
                    "paused"
                } else {
                    "backend_unavailable"
                },
                reason: semantic_backend.last_error,
                since_ms: semantic_backend.since_ms,
                next_retry_ms: semantic_backend.next_retry_ms,
                paused_until_ms: semantic_backend.paused_until_ms,
                stage: None,
                embedded_chunks: None,
                total_chunks: None,
                current_batch: None,
                total_batches: None,
                fill: None,
            }
        } else {
            match &*semantic_status {
                SemanticIndexStatus::Ready { .. } => SemanticHealthComponentSnapshot {
                    // A ready index with a large slice of its files masked for
                    // re-embedding still answers queries, but not for those
                    // files; see `SemanticIndexStatus::mass_refresh_pending`.
                    status: if semantic_status.mass_refresh_pending().is_some() {
                        "refreshing"
                    } else {
                        "ready"
                    },
                    reason: None,
                    since_ms: None,
                    next_retry_ms: None,
                    paused_until_ms: None,
                    stage: None,
                    embedded_chunks: None,
                    total_chunks: None,
                    current_batch: None,
                    total_batches: None,
                    fill: None,
                },
                SemanticIndexStatus::Building { stage, .. } => {
                    let progress = semantic_build_progress
                        .as_ref()
                        .map(SemanticBuildProgress::snapshot);
                    SemanticHealthComponentSnapshot {
                        status: "building",
                        reason: None,
                        since_ms: None,
                        next_retry_ms: None,
                        paused_until_ms: None,
                        stage: Some(stage.clone()),
                        embedded_chunks: progress.as_ref().map(|progress| progress.embedded_chunks),
                        total_chunks: progress.as_ref().map(|progress| progress.total_chunks),
                        current_batch: progress.as_ref().map(|progress| progress.current_batch),
                        total_batches: progress.as_ref().map(|progress| progress.total_batches),
                        fill: None,
                    }
                }
                SemanticIndexStatus::Disabled => SemanticHealthComponentSnapshot {
                    status: "disabled",
                    reason: None,
                    since_ms: None,
                    next_retry_ms: None,
                    paused_until_ms: None,
                    stage: None,
                    embedded_chunks: None,
                    total_chunks: None,
                    current_batch: None,
                    total_batches: None,
                    fill: None,
                },
                SemanticIndexStatus::Failed(message)
                    if crate::semantic_admission::is_not_opened_status(message) =>
                {
                    SemanticHealthComponentSnapshot {
                        status: crate::semantic_admission::NOT_OPENED_LABEL,
                        reason: None,
                        since_ms: None,
                        next_retry_ms: None,
                        paused_until_ms: None,
                        stage: None,
                        embedded_chunks: None,
                        total_chunks: None,
                        current_batch: None,
                        total_batches: None,
                        fill: None,
                    }
                }
                SemanticIndexStatus::Failed(_) => SemanticHealthComponentSnapshot {
                    status: "degraded",
                    reason: None,
                    since_ms: None,
                    next_retry_ms: None,
                    paused_until_ms: None,
                    stage: None,
                    embedded_chunks: None,
                    total_chunks: None,
                    current_batch: None,
                    total_batches: None,
                    fill: None,
                },
            }
        };
        // The data layer's meaning plane, once it reports for the root,
        // stands in for today's semantic index, as on the status tool. An
        // unreachable embedding backend still reports as such: no fill can
        // embed either.
        if semantic_backend.available {
            if let Some(meaning) = data_store
                .as_ref()
                .and_then(|report| report.meaning.as_ref())
            {
                semantic_index = meaning.health();
            }
        }
        let callgraph_writer = self.callgraph_writer.load(Ordering::SeqCst);
        if cache_unavailable {
            semantic_index.status = crate::root_cache::CACHE_DIR_UNAVAILABLE;
            semantic_index.reason = Some(crate::root_cache::CACHE_DIR_UNAVAILABLE.into());
        }
        let callgraph_store_status = if cache_unavailable {
            crate::root_cache::CACHE_DIR_UNAVAILABLE
        } else if !heavy_root_work_allowed {
            "disabled"
        } else if !callgraph_writer && config.indexes.callgraph {
            // Read-only roots never cold-build; they query the shared store
            // via ReadonlyCallGraphStore on demand.
            "ready"
        } else if config.indexes.callgraph {
            // Either a build receiver is installed or the build has not been
            // admitted yet; both resolve to ready under this configuration.
            "building"
        } else {
            // A configure that disables the store publishes the new config
            // before it retires the previous generation's build receiver.
            // That lingering receiver must not report "building": no build it
            // describes can ever publish into a disabled configuration.
            "disabled"
        };
        // The whole-repository analyses run only inside an `aft_inspect` call,
        // so this plane has no stale state: it is building while such a run is
        // registered, and otherwise ready to run one on request.
        let tier2_status = if cache_unavailable {
            crate::root_cache::CACHE_DIR_UNAVAILABLE
        } else if tier2_builder_busy {
            "building"
        } else if !config.inspect.enabled || !heavy_root_work_allowed {
            "disabled"
        } else {
            "ready"
        };

        let (search_index_status, callgraph_store_status) = match &data_store {
            Some(report) => (report.text.status, report.callgraph.status),
            None => (search_index_status, callgraph_store_status),
        };
        RootHealthSummary {
            cache_unavailable: cache_unavailable_reason,
            state: RootHealthState::Ready,
            search_index_status: Some(search_index_status),
            semantic_index: Some(semantic_index),
            callgraph_store_status: Some(callgraph_store_status),
            view_load_failures,
            view_load_failure_last,
            view_storage_reserve,
            tier2_status: Some(tier2_status),
            bash: Some(bash),
            suspended_domains,
            data_store: data_store.map(|report| report.health()),
        }
    }

    pub fn try_health_snapshot(&self, project_root: &Path) -> RootHealthSnapshot {
        self.try_health_summary().into_snapshot(project_root)
    }

    /// Records `counts` as the last bar shown to the agent and reports whether
    /// it differs from the previous one. Call only when the bar is about to be
    /// rendered into agent-visible text: a call that does not render would
    /// swallow the change.
    pub fn should_emit_status_bar(&self, counts: &StatusBarCountValues) -> bool {
        self.should_emit_status_bar_for_session(crate::protocol::DEFAULT_SESSION_ID, counts)
    }

    pub fn should_emit_status_bar_for_session(
        &self,
        session_id: &str,
        counts: &StatusBarCountValues,
    ) -> bool {
        self.status_bar_last_emitted.should_emit(session_id, counts)
    }

    /// Record an atomic batch of complete per-producer diagnostics observations.
    /// Callers must construct the batch from a source that proved the document
    /// version; passive count reads have no access to this mutation boundary.
    pub fn accept_alert_observation_batch(
        &self,
        batch: &AcceptedObservationBatch,
    ) -> Result<Vec<AcceptedObservationResult>, ObservationError> {
        self.alert_state.lock().accept_batch(batch)
    }

    /// Invalidate the status-bar tsconfig-membership cache. Called from the
    /// watcher seam when a tsconfig-like file changes and from `configure`
    /// when the project root changes, so the next bar count re-reads from disk.
    pub fn clear_tsconfig_membership_cache(&self) {
        self.tsconfig_membership.lock().clear();
    }

    #[cfg(test)]
    pub fn tsconfig_membership_clear_generation_for_test(&self) -> u64 {
        self.tsconfig_membership.lock().generation()
    }

    /// Refresh the cached TODO count for the status bar. `None` preserves the
    /// last-known value (the category wasn't recomputed or has no real
    /// aggregate yet) so we never overwrite a real count with a fabricated `0`.
    pub fn update_status_bar_todos(&self, todos: Option<usize>) {
        let Some(todos) = todos else {
            return;
        };
        let mut cached = self
            .status_bar_todos
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if cached.todos != Some(todos) {
            cached.todos = Some(todos);
            cached.generation = cached.generation.wrapping_add(1);
        }
    }

    /// Borrow the cached project gitignore matcher. Returns `None` when no
    /// project_root is configured or when the project has no gitignore files.
    pub fn gitignore(&self) -> Option<Arc<ignore::gitignore::Gitignore>> {
        self.gitignore
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Shared gitignore matcher handle for the watcher filter thread.
    pub fn shared_gitignore(&self) -> SharedGitignore {
        Arc::clone(&self.gitignore)
    }

    /// Monotonic generation bumped after every matcher rebuild/clear. The
    /// watcher filter thread uses it to wait until the main thread has rebuilt
    /// ignore rules after it reports an ignore-file change.
    pub fn gitignore_generation(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.gitignore_generation)
    }

    /// Whether any matcher (including a deliberate "no matcher") has been
    /// published for this context since it was created.
    ///
    /// Every publication path (`rebuild_gitignore`, `clear_gitignore`, and the
    /// unchanged-input acknowledgement that only runs after an earlier build)
    /// bumps the generation, so generation zero means the ignore rules were
    /// never loaded. A `None` matcher at generation zero is "not known yet",
    /// not "this project ignores nothing".
    pub(crate) fn gitignore_published(&self) -> bool {
        self.gitignore_generation.load(Ordering::SeqCst) > 0
    }

    fn set_gitignore(&self, matcher: Option<Arc<ignore::gitignore::Gitignore>>) {
        *self
            .gitignore
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = matcher;
        let generation = self.gitignore_generation.load(Ordering::SeqCst) + 1;
        self.retire_ignored_pending_paths(generation);
        self.gitignore_generation.fetch_add(1, Ordering::SeqCst);
    }

    fn retire_ignored_pending_paths(&self, generation: u64) {
        let keep = |path: &PathBuf| {
            !crate::watcher_filter::queued_path_is_ignored_by_matcher(&self.gitignore, path)
        };
        let retire = |pending: &mut BTreeSet<PathBuf>| {
            let before = pending.len();
            pending.retain(&keep);
            before - pending.len()
        };
        let semantic = retire(&mut self.pending_semantic_index_paths.lock());
        let tier2 = retire(&mut self.pending_tier2_paths.lock());
        let watcher = self
            .watcher_drain_slice
            .lock()
            .as_mut()
            .map_or(0, |state| state.retire_ignored_paths(&keep));
        crate::slog_info!(
            "ignore queue retirement: generation={} watcher={} callgraph_pending={} callgraph_queued={} tier2={} semantic_pending={}",
            generation, watcher, 0, 0, tier2, semantic
        );
    }

    /// Rebuild the gitignore matcher from the current `project_root` and
    /// cache it. Called by the configure handler whenever the project root
    /// changes, and by the watcher event drain when a `.gitignore` file
    /// itself is modified.
    ///
    /// The builder honors:
    /// - `<project_root>/.gitignore`
    /// - Git's global excludes file (the same source used by `ignore::WalkBuilder`)
    /// - the repository's real `info/exclude` file, resolved through Git's
    ///   common dir for linked worktrees
    /// - nested `.gitignore` files (each `.gitignore` discovered during
    ///   the recursive walk)
    ///
    /// Stores `None` if there's no project_root or no matchable gitignore
    /// files. Logs build errors but never fails configure.
    /// Clear any cached gitignore matcher without rebuilding.
    ///
    /// Used by `handle_configure` in degraded mode (e.g. `project_root == $HOME`)
    /// where running the gitignore-discovery walk would exceed the configure
    /// budget. The watcher event filter falls back to the hardcoded infra-dir
    /// skip list when no matcher is present.
    pub fn clear_gitignore(&self) {
        *self.gitignore_inputs.lock() = None;
        self.set_gitignore(None);
    }

    pub fn rebuild_gitignore(&self) {
        let _ = self.rebuild_gitignore_inner(None);
    }

    pub(crate) fn rebuild_gitignore_after_changes(
        &self,
        changed_paths: &[PathBuf],
    ) -> IgnoreRuleChange {
        self.rebuild_gitignore_inner(Some(changed_paths))
    }

    pub(crate) fn start_gitignore_rescan(
        &self,
    ) -> crossbeam_channel::Receiver<PreparedGitignoreRebuild> {
        let root = self
            .config()
            .project_root
            .clone()
            .expect("configured watcher root");
        let common_dir = self.git_common_dir.lock().clone();
        let previous = self.gitignore_inputs.lock().clone();
        let old_matcher = self.gitignore();
        let (tx, rx) = crossbeam_channel::bounded(1);
        std::thread::Builder::new()
            .name("aft-ignore-snapshot".into())
            .spawn(move || {
                let root = std::fs::canonicalize(&root).unwrap_or(root);
                #[cfg(test)]
                {
                    let mut slot = IGNORE_RESCAN_GATE.get_or_init(Default::default).lock();
                    let gate = if slot.as_ref().is_some_and(|gate| gate.root == root) {
                        slot.take()
                    } else {
                        None
                    };
                    drop(slot);
                    if let Some(gate) = gate {
                        let _ = gate.reached.send(());
                        let _ = gate.release.recv_timeout(Duration::from_secs(5));
                    }
                }
                let inputs =
                    gitignore_state::IgnoreInputSnapshot::collect(root, common_dir.as_deref());
                let (matcher, change) = if inputs.has_same_effective_inputs(previous.as_ref()) {
                    (old_matcher, IgnoreRuleChange::Unchanged)
                } else {
                    let matcher = inputs.build_matcher();
                    let change = inputs.classify_against(
                        previous.as_ref(),
                        old_matcher.as_deref(),
                        matcher.as_deref(),
                    );
                    (matcher, change)
                };
                let _ = tx.send(PreparedGitignoreRebuild {
                    inputs,
                    matcher,
                    change,
                });
            })
            .expect("spawn ignore snapshot worker");
        rx
    }

    pub(crate) fn publish_gitignore_rescan(&self, prepared: PreparedGitignoreRebuild) {
        if matches!(prepared.change, IgnoreRuleChange::Unchanged) {
            self.gitignore_generation.fetch_add(1, Ordering::SeqCst);
        } else {
            self.gitignore_matcher_rebuilds
                .fetch_add(1, Ordering::SeqCst);
            *self.gitignore_inputs.lock() = Some(prepared.inputs);
            self.set_gitignore(prepared.matcher);
        }
    }

    fn rebuild_gitignore_inner(&self, changed_paths: Option<&[PathBuf]>) -> IgnoreRuleChange {
        let root_raw = match self.config().project_root.clone() {
            Some(root) => root,
            None => {
                *self.gitignore_inputs.lock() = None;
                self.set_gitignore(None);
                return IgnoreRuleChange::Full { scopes: Vec::new() };
            }
        };
        // Watcher paths are canonicalized before dispatch. Keep the matcher's
        // root in the same spelling so the ignore crate never receives a path
        // outside its lexical root on platforms with root aliases.
        let root = std::fs::canonicalize(&root_raw).unwrap_or(root_raw);
        let previous_inputs = self.gitignore_inputs.lock().clone();
        if changed_paths.is_some_and(|paths| {
            previous_inputs
                .as_ref()
                .is_some_and(|inputs| inputs.changed_paths_match_bytes(paths))
        }) {
            // The watcher thread waits on this generation before it re-filters
            // the batch. A byte-identical write still needs an acknowledgement,
            // but it must not publish or rebuild a matcher.
            self.gitignore_generation.fetch_add(1, Ordering::SeqCst);
            return IgnoreRuleChange::Unchanged;
        }

        let git_common_dir = self.git_common_dir.lock().clone();
        #[cfg(test)]
        IGNORE_WALKS_ON_THREAD.with(|walks| walks.set(walks.get() + 1));
        let inputs = gitignore_state::IgnoreInputSnapshot::collect(root, git_common_dir.as_deref());
        if inputs.has_same_effective_inputs(previous_inputs.as_ref()) {
            self.gitignore_generation.fetch_add(1, Ordering::SeqCst);
            return IgnoreRuleChange::Unchanged;
        }

        self.gitignore_matcher_rebuilds
            .fetch_add(1, Ordering::SeqCst);
        let old_matcher = self.gitignore();
        let matcher = inputs.build_matcher();
        if let Some(matcher) = &matcher {
            crate::slog_info!(
                "gitignore matcher built: {} pattern(s)",
                matcher.num_ignores()
            );
        }
        let change = inputs.classify_against(
            previous_inputs.as_ref(),
            old_matcher.as_deref(),
            matcher.as_deref(),
        );
        *self.gitignore_inputs.lock() = Some(inputs);
        self.set_gitignore(matcher);
        change
    }

    #[doc(hidden)]
    pub fn gitignore_matcher_rebuild_count_for_test(&self) -> u64 {
        self.gitignore_matcher_rebuilds.load(Ordering::SeqCst)
    }

    /// How many ignore-file walks the calling thread has run, across every
    /// context. Lets a test prove a path never walks a project on the thread
    /// that called it.
    #[cfg(test)]
    pub(crate) fn ignore_walks_on_current_thread_for_test() -> u64 {
        IGNORE_WALKS_ON_THREAD.with(std::cell::Cell::get)
    }

    /// Shared atomic mirror of `experimental.bash.compress`. Updated by the
    /// configure handler. Read by the BgTaskRegistry compressor closure.
    pub fn bash_compress_flag(&self) -> Arc<std::sync::atomic::AtomicBool> {
        Arc::clone(&self.bash_compress_flag)
    }

    /// Update the shared `bash_compress_flag` mirror. Call this from the
    /// configure handler whenever `experimental.bash.compress` changes so the
    /// BgTaskRegistry watchdog sees the new value on the next completion.
    pub fn sync_bash_compress_flag(&self) {
        let value = self.config().experimental_bash_compress;
        self.bash_compress_flag
            .store(value, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn set_bash_compress_enabled(&self, enabled: bool) {
        self.update_config(|config| {
            config.experimental_bash_compress = enabled;
        });
        self.bash_compress_flag
            .store(enabled, std::sync::atomic::Ordering::Relaxed);
    }

    /// Read-only access to the TOML filter registry, building it lazily on
    /// first use. Returns an `RwLockReadGuard` that callers can `lookup`
    /// against directly.
    pub fn filter_registry(
        &self,
    ) -> std::sync::RwLockReadGuard<'_, crate::compress::toml_filter::FilterRegistry> {
        self.ensure_filter_registry_loaded();
        match self.filter_registry.read() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Returns the shared `Arc<RwLock<FilterRegistry>>` handle so threads
    /// outside `AppContext` (notably the bash watchdog) can read it without
    /// touching the rest of the context.
    pub fn shared_filter_registry(&self) -> crate::compress::SharedFilterRegistry {
        self.ensure_filter_registry_loaded();
        Arc::clone(&self.filter_registry)
    }

    /// Force a fresh load of the TOML filter registry. Called when configure
    /// changes the project root, storage_dir, or trust state so subsequent
    /// `compress::compress` calls pick up new filters.
    pub fn reset_filter_registry(&self) {
        let new_registry = crate::compress::build_registry_for_context(self);
        self.filter_registry_rebuild_count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let mut slot = self
            .filter_registry
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *slot = new_registry;
        self.filter_registry_loaded
            .store(true, std::sync::atomic::Ordering::Release);
    }

    fn ensure_filter_registry_loaded(&self) {
        use std::sync::atomic::Ordering;
        if self.filter_registry_loaded.load(Ordering::Acquire) {
            return;
        }
        // Build outside the lock to avoid blocking other readers during a
        // multi-file TOML parse.
        let new_registry = crate::compress::build_registry_for_context(self);
        self.filter_registry_rebuild_count
            .fetch_add(1, Ordering::SeqCst);
        self.install_lazy_filter_registry(new_registry);
    }

    fn install_lazy_filter_registry(
        &self,
        new_registry: crate::compress::toml_filter::FilterRegistry,
    ) {
        let mut slot = self
            .filter_registry
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // A reset may have installed newer filters while the lazy build was parsing.
        // Test and publish under the same lock as reset's loaded flag.
        if !self.filter_registry_loaded.load(Ordering::Acquire) {
            *slot = new_registry;
            self.filter_registry_loaded.store(true, Ordering::Release);
        }
    }

    #[cfg(test)]
    pub fn filter_registry_rebuild_count_for_test(&self) -> u64 {
        self.filter_registry_rebuild_count.load(Ordering::SeqCst)
    }

    /// Whether the compiled filter registry is resident.
    #[cfg(test)]
    pub(crate) fn filter_registry_loaded(&self) -> bool {
        self.filter_registry_loaded.load(Ordering::Acquire)
    }

    /// Take the compiled filters of an idle root out of the shared slot. They
    /// are rebuilt from the same sources by the next
    /// [`Self::filter_registry`] or [`Self::shared_filter_registry`] call, or
    /// by [`Self::reload_released_filter_registry`] when a background bash
    /// task is spawned. This releases private tables and overrides; immutable
    /// compiled builtin regexes remain shared by the process.
    pub(crate) fn take_filter_registry(&self) -> crate::compress::toml_filter::FilterRegistry {
        let mut slot = self
            .filter_registry
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Cleared under the same write lock that publishes the flag
        // everywhere else, so the slot and the flag never disagree.
        self.filter_registry_loaded.store(false, Ordering::Release);
        self.filter_registry_released.store(true, Ordering::Release);
        std::mem::take(&mut *slot)
    }

    /// Load the filters again if idle eviction took them. A background bash
    /// task's output is compressed on the watchdog thread through the shared
    /// handle, which reads the registry as it is and cannot load it. A root
    /// whose registry was never released keeps the cost of its first load
    /// where it was (configure, or the first compression).
    pub(crate) fn reload_released_filter_registry(&self) {
        if self.filter_registry_released.swap(false, Ordering::AcqRel) {
            self.ensure_filter_registry_loaded();
        }
    }

    pub fn app(&self) -> Arc<App> {
        Arc::clone(&self.app)
    }

    /// Clone the LSP child registry handle. Used by main.rs to give the
    /// signal handler thread a way to SIGKILL LSP children on shutdown.
    pub fn lsp_child_registry(&self) -> crate::lsp::child_registry::LspChildRegistry {
        self.app.lsp_child_registry()
    }

    pub fn stdout_writer(&self) -> SharedStdoutWriter {
        self.app.stdout_writer()
    }

    pub fn set_progress_sender(&self, sender: Option<ProgressSender>) {
        if let Ok(mut progress_sender) = self.progress_sender.lock() {
            *progress_sender = sender;
        }
    }

    pub fn emit_progress(&self, frame: ProgressFrame) {
        let progress_sender = self.progress_sender_handle();
        if let Some(sender) = progress_sender.as_ref() {
            sender(PushFrame::Progress(frame));
        }
    }

    pub fn status_emitter(&self) -> &StatusEmitter {
        &self.status_emitter
    }

    pub(crate) fn install_fleet_status_client(
        &self,
        client: Option<crate::fleet_status::FleetStatusClient>,
    ) {
        *self
            .fleet_status_client
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = client;
    }

    pub(crate) fn fleet_status_client(&self) -> Option<crate::fleet_status::FleetStatusClient> {
        self.fleet_status_client
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Get a clone of the current progress sender for use from background
    /// threads. Returns `None` when the main loop hasn't installed one (tests,
    /// CLI without push frames).
    ///
    /// Used by `configure`'s deferred file-walk thread to push warnings after
    /// configure has already returned, so configure latency stays sub-100 ms
    /// even on huge directories.
    pub fn progress_sender_handle(&self) -> Option<ProgressSender> {
        let sender = self
            .progress_sender
            .lock()
            .ok()
            .and_then(|sender| sender.clone())?;
        let origin = REQUEST_PROGRESS_ORIGIN.with(|origin| origin.borrow().clone());
        Some(Arc::new(Box::new(move |mut frame| {
            if let PushFrame::Progress(progress) = &mut frame {
                progress.origin = origin.as_ref().and_then(|(request_id, route)| {
                    (request_id == &progress.request_id).then_some(*route)
                });
            }
            sender(frame);
        })))
    }

    pub fn advance_configure_generation(&self) -> u64 {
        let generation = self
            .subc_lifecycle
            .advance_generation(self.configure_generation.as_ref());
        generation
    }

    pub(crate) fn mark_subc_bound(&self) {
        self.lsp().resume();
        self.subc_lifecycle.mark_bound();
        // Idle release holds the lifecycle gate while clearing the pools. Once
        // mark_bound returns, an earlier release has finished and later releases
        // skip this root. Repair even if configure committed before the release.
        self.attach_ready_database_pools(&self.database_open.lock());
        self.inspect_manager.resume_root_work();
    }

    pub(crate) fn mark_subc_unbound(&self) {
        self.subc_lifecycle
            .mark_unbound(self.configure_generation.as_ref());
        crate::provisional_identity_retry::cancel(self);
        self.repeat_breaker.clear();
    }

    #[doc(hidden)]
    pub fn subc_unbound_quiesced(&self) -> bool {
        self.subc_lifecycle.is_unbound()
    }

    pub(crate) fn try_subc_unbound_quiesced(&self) -> Option<bool> {
        self.subc_lifecycle.try_is_bound().map(|bound| !bound)
    }

    #[cfg(test)]
    pub(crate) fn set_unbound_build_abandon_grace_for_test(&self, grace: Duration) {
        self.subc_lifecycle.set_abandon_grace_for_test(grace);
    }

    pub(crate) fn subc_lifecycle_admission(&self) -> SubcLifecycleAdmission {
        self.subc_lifecycle.clone()
    }

    pub(crate) fn run_if_subc_bound_generation<R>(
        &self,
        expected_generation: u64,
        action: impl FnOnce() -> R,
    ) -> Option<R> {
        if self
            .canonical_cache_root_opt()
            .or_else(|| self.config().project_root.clone())
            .is_some_and(|root| !root.is_dir())
        {
            return None;
        }
        self.subc_lifecycle.run_if_current(
            self.configure_generation.as_ref(),
            expected_generation,
            action,
        )
    }

    /// After confirming the project root is gone, this method drops its watcher
    /// and shuts down its language-server clients.
    /// Returns true when the drain should stop, including while a root check is
    /// inconclusive or cleanup must be retried on a later drain call.
    pub(crate) fn try_cleanup_deleted_root(&self, expected_generation: u64) -> bool {
        self.try_cleanup_deleted_root_with_probe_impl(expected_generation, |path| {
            std::fs::metadata(path)
        })
    }

    // Only the Unix-only idle language-server tests inject a probe.
    #[cfg(all(test, unix))]
    pub(crate) fn try_cleanup_deleted_root_with_probe(
        &self,
        expected_generation: u64,
        root_metadata: impl FnOnce(&Path) -> std::io::Result<std::fs::Metadata>,
    ) -> bool {
        self.try_cleanup_deleted_root_with_probe_impl(expected_generation, root_metadata)
    }

    fn try_cleanup_deleted_root_with_probe_impl(
        &self,
        expected_generation: u64,
        root_metadata: impl FnOnce(&Path) -> std::io::Result<std::fs::Metadata>,
    ) -> bool {
        self.subc_lifecycle
            .run_if_current(
                self.configure_generation.as_ref(),
                expected_generation,
                || {
                    let Some(root) = self
                        .canonical_cache_root_opt()
                        .or_else(|| self.config().project_root.clone())
                    else {
                        return false;
                    };
                    let mut cleanup = self.deleted_root_cleanup.lock();
                    match root_metadata(&root) {
                        Ok(metadata) if metadata.is_dir() => {
                            *cleanup = None;
                            return false;
                        }
                        Ok(_) => {}
                        Err(error)
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                            ) => {}
                        Err(error) => {
                            let already_logged = matches!(
                                *cleanup,
                                Some(DeletedRootCleanup::Inconclusive(generation))
                                    if generation == expected_generation
                            );
                            if !already_logged {
                                crate::slog_warn!(
                                    "project root check inconclusive {}: {}; preserving watcher and language servers",
                                    root.display(),
                                    error
                                );
                            }
                            *cleanup = Some(DeletedRootCleanup::Inconclusive(expected_generation));
                            return true;
                        }
                    }
                    if matches!(*cleanup, Some(DeletedRootCleanup::Complete(generation)) if generation == expected_generation) {
                        return true;
                    }
                    *cleanup = Some(DeletedRootCleanup::Pending(expected_generation));
                    // A concurrent drain may hold the receiver while awaiting
                    // lifecycle admission. Never wait for it under this gate.
                    if !self.try_stop_watcher_runtime_in_background() {
                        return true;
                    }
                    let Some(mut lsp) = self.try_lsp() else {
                        // Directory absence survives watcher shutdown. The next
                        // drain retries even though no receiver remains to wake it.
                        return true;
                    };
                    // Also invalidate starts that have not published a client yet.
                    let clients = lsp.take_all_clients();
                    drop(lsp);
                    *cleanup = Some(DeletedRootCleanup::Complete(expected_generation));
                    let _ = self.add_degraded_reason("project_root_deleted".to_string());
                    crate::slog_warn!(
                        "project root deleted {}: shut down {} language server(s); dropping watcher",
                        root.display(),
                        clients.len()
                    );
                    LspManager::spawn_idle_lsp_reap(clients);
                    self.signal_status_changed();
                    true
                },
            )
            .unwrap_or(false)
    }

    /// Commit the warm-maintenance identity for a successful configure.
    ///
    /// The semantic epoch decision shares the warm-state lock with equivalence
    /// detection. A configure that prepared its input comparison before another
    /// matching configure committed can therefore adopt the matching generation
    /// and semantic worker instead of invalidating that worker from stale input
    /// snapshots.
    pub fn note_configure_warm_key(
        &self,
        key: String,
        semantic_build_inputs_changed: bool,
    ) -> (u64, bool) {
        let mut state = self.configure_warm_state.lock();
        let equivalent = state.key.as_ref().is_some_and(|previous| *previous == key);
        let generation = if equivalent {
            self.configure_generation()
        } else {
            self.configure_content_generation
                .fetch_add(1, Ordering::SeqCst);
            self.advance_configure_generation()
        };
        if !equivalent && semantic_build_inputs_changed {
            self.advance_semantic_build_epoch();
        }
        state.generation = generation;
        state.key = Some(key);
        (generation, equivalent)
    }

    pub(crate) fn configure_warm_key_matches(&self, key: &str) -> bool {
        self.configure_warm_state
            .lock()
            .key
            .as_deref()
            .is_some_and(|current| current == key)
    }

    pub(crate) fn invalidate_configure_warm_state(&self) {
        self.configure_warm_state.lock().key = None;
    }

    pub fn note_configure_session_binding(&self, root: PathBuf, session_id: String) -> bool {
        #[cfg(test)]
        record_configure_bind_effect_for_test(&root, &session_id, ConfigureBindEffect::Session);
        self.configured_session_roots
            .lock()
            .insert((root, session_id))
    }

    pub(crate) fn has_configure_session_binding(&self, root: &Path, session_id: &str) -> bool {
        self.configured_session_roots
            .lock()
            .contains(&(root.to_path_buf(), session_id.to_string()))
    }

    /// Undo [`Self::note_configure_session_binding`] when the maintenance job
    /// carrying the session's bash replay was dropped as stale: the session has
    /// not actually been replayed, so its next bind must count as first again.
    pub fn forget_configure_session_binding(&self, root: &Path, session_id: &str) {
        self.configured_session_roots
            .lock()
            .remove(&(root.to_path_buf(), session_id.to_string()));
        self.repeat_breaker.clear_session(session_id);
    }

    pub fn repeat_breaker(&self) -> &crate::repeat_state::RepeatBreaker {
        &self.repeat_breaker
    }

    /// Cheap emptiness probes for the maintenance scheduler: a drain kind with
    /// no pending work is not enqueued at all, so idle roots stop paying a
    /// dispatch cycle per kind per tick. Subsystem probes are lock-free or
    /// try-lock (a contended source reports "maybe work" and the kind is
    /// enqueued). The view verification registry waits only for map metadata,
    /// so unrelated views' registry lookups do not count as pending work.
    ///
    /// These probes run on the subc frame loop, which also answers health
    /// checks and routes every request. Never wait for subsystem work here:
    /// use `try_lock` and treat contention as pending work. The verification
    /// registry lock must never cover per-view locks or I/O.
    pub fn watcher_drain_has_work(&self) -> bool {
        let cleanup_pending = self.deleted_root_cleanup.try_lock().is_none_or(|cleanup| {
            matches!(
                *cleanup,
                Some(DeletedRootCleanup::Pending(generation) | DeletedRootCleanup::Inconclusive(generation))
                    if generation == self.configure_generation()
            )
        });
        let receiver_pending = self
            .watcher_rx
            .try_lock()
            .is_none_or(|slot| slot.as_ref().is_some_and(|rx| !rx.is_empty()));
        cleanup_pending
            || receiver_pending
            || self.watcher_drain_slice.try_lock().is_none_or(|slot| {
                slot.as_ref().is_some_and(|state| {
                    state.has_pending_work()
                        || state
                            .ignore_refresh_due
                            .is_some_and(|due| Instant::now() >= due)
                })
            })
    }

    pub fn lsp_drain_has_work(&self) -> bool {
        match self.lsp_manager.try_lock() {
            Some(lsp) => lsp.has_pending_events(),
            // Contended: the manager is busy, so events may be queuing.
            None => true,
        }
    }

    /// A drain takes its continuation out of the slot while applying it, so
    /// an empty slot alone does not prove that indexed answers are current.
    ///
    /// The reads run while a drain on another thread may move work from the
    /// dispatch channel into its continuation and back into the slot. A drain
    /// advances `watcher_drain_activity` when it starts and when it ends, so
    /// reading it before and after the channel, slot and in-flight count
    /// catches any drain that moved work between those reads.
    /// Events the watcher thread holds for a full dispatch channel are
    /// pending too, even while the channel itself is momentarily empty.
    pub fn watcher_query_has_pending_changes(&self) -> bool {
        let activity = self.watcher_drain_activity.load(Ordering::Acquire);
        // Directory discovery publishes children to the flush window before
        // ending its pending guard; dispatch publishes before clearing that
        // window or its backlog. Read these stages before the channel below.
        let intake_pending = match self.watcher_counters.try_read() {
            Ok(counters) => counters.query_delivery_pending(),
            Err(std::sync::TryLockError::Poisoned(counters)) => {
                counters.into_inner().query_delivery_pending()
            }
            // Contended: assume events may be held, as the probes below do.
            Err(std::sync::TryLockError::WouldBlock) => true,
        };
        intake_pending
            || self.watcher_drain_has_work()
            || self.watcher_drains_in_flight.load(Ordering::Acquire) > 0
            || self.watcher_drain_activity.load(Ordering::Acquire) != activity
    }

    /// Changes still held by the watcher when an answer starts, including
    /// root-relative call-graph paths when requested. The index store waits for
    /// or discloses its own pending edits in the query path.
    pub fn pending_changes_for_query(&self, include_callgraph: bool) -> QueryPendingChanges {
        let include_callgraph = include_callgraph && self.config().indexes.callgraph;
        let unflushed = if include_callgraph {
            self.watcher_counters().unflushed_paths()
        } else {
            BTreeSet::new()
        };
        #[cfg(test)]
        pending_capture_boundary_for_test(PendingCaptureBoundary::AfterFlushWindow);
        let watcher = self.watcher_query_has_pending_changes();
        let mut changes = CallgraphUnappliedChanges::default();
        if include_callgraph {
            let roots: Vec<PathBuf> = [
                self.callgraph_project_root(),
                self.config().project_root.clone(),
            ]
            .into_iter()
            .flatten()
            .collect();
            changes.store_paths.extend(unflushed.iter().map(|path| {
                roots
                    .iter()
                    .find_map(|root| path.strip_prefix(root).ok())
                    .unwrap_or(path)
                    .to_string_lossy()
                    .replace('\\', "/")
            }));
        }
        QueryPendingChanges {
            watcher,
            callgraph: changes,
        }
    }

    pub(crate) fn watcher_drain_guard(&self) -> WatcherDrainGuard<'_> {
        self.watcher_drains_in_flight.fetch_add(1, Ordering::AcqRel);
        self.watcher_drain_activity.fetch_add(1, Ordering::AcqRel);
        WatcherDrainGuard {
            in_flight: &self.watcher_drains_in_flight,
            activity: &self.watcher_drain_activity,
        }
    }

    pub fn completion_drains_have_work(&self) -> bool {
        if self.semantic_index_rx.try_lock().is_none_or(|slot| {
            slot.as_ref().is_some_and(|receiver| {
                !receiver.is_empty()
                    || self.semantic_index_rx_terminal_epoch.load(Ordering::SeqCst)
                        == self.semantic_index_rx_epoch()
            })
        }) {
            return true;
        }
        match self.semantic_refresh_event_rx.try_lock() {
            None => return true,
            Some(slot) => {
                if slot.as_ref().is_some_and(|rx| !rx.is_empty())
                    || (self.semantic_refresh_probe_ready() && slot.is_some())
                {
                    return true;
                }
            }
        }
        if self.semantic_refresh_worker.try_lock().is_none_or(|slot| {
            slot.as_ref()
                .is_some_and(|worker_slot| match worker_slot.try_lock() {
                    Ok(handle) => handle
                        .as_ref()
                        .is_some_and(std::thread::JoinHandle::is_finished),
                    Err(std::sync::TryLockError::WouldBlock) => true,
                    Err(std::sync::TryLockError::Poisoned(_)) => true,
                })
        }) {
            return true;
        }
        self.inspect_manager().has_pending_completions()
    }

    pub fn configure_tail_has_work(&self) -> bool {
        self.configure_maintenance_jobs
            .try_lock()
            .is_none_or(|jobs| !jobs.is_empty())
            || self
                .parked_configure_tail
                .try_lock()
                .is_none_or(|tail| tail.as_ref().is_some_and(|state| state.has_ready_job()))
            || !self.configure_warnings_rx.is_empty()
            || self.provisional_identity_retry.is_due()
    }

    /// No configure-tail work is queued or parked. The background identity
    /// retry replays a bind only then, so it never interleaves with a tail.
    pub(crate) fn configure_tail_idle(&self) -> bool {
        self.configure_maintenance_jobs.lock().is_empty()
            && self
                .parked_configure_tail
                .lock()
                .as_ref()
                .is_none_or(|tail| tail.jobs_is_empty())
    }

    pub(crate) fn provisional_identity_retry(
        &self,
    ) -> &crate::provisional_identity_retry::RetrySlot {
        &self.provisional_identity_retry
    }

    /// Hold the lock behind one maintenance probe for the duration of
    /// `while_held`, so a test can prove the probe does not wait for it.
    #[cfg(test)]
    pub(crate) fn hold_maintenance_probe_lock_for_test(
        &self,
        lock: MaintenanceProbeLock,
        while_held: impl FnOnce(),
    ) {
        match lock {
            MaintenanceProbeLock::WatcherReceiver => {
                let _held = self.watcher_rx.lock();
                while_held();
            }
            MaintenanceProbeLock::WatcherDrainSlice => {
                let _held = self.watcher_drain_slice.lock();
                while_held();
            }
            MaintenanceProbeLock::Tier2RefreshScheduler => {
                let _held = self.tier2_refresh_scheduler.lock();
                while_held();
            }
            MaintenanceProbeLock::LspManager => {
                let _held = self.lsp_manager.lock();
                while_held();
            }
            MaintenanceProbeLock::ConfigureMaintenanceJobs => {
                let _held = self.configure_maintenance_jobs.lock();
                while_held();
            }
            MaintenanceProbeLock::ParkedConfigureTail => {
                let _held = self.parked_configure_tail.lock();
                while_held();
            }
            MaintenanceProbeLock::SemanticIndexReceiver => {
                let _held = self.semantic_index_rx.lock();
                while_held();
            }
            MaintenanceProbeLock::SemanticRefreshEvents => {
                let _held = self.semantic_refresh_event_rx.lock();
                while_held();
            }
            MaintenanceProbeLock::SemanticRefreshWorker => {
                let _held = self.semantic_refresh_worker.lock();
                while_held();
            }
        }
    }

    pub(crate) fn park_configure_tail(
        &self,
        state: crate::commands::configure::ConfigureMaintenanceState,
    ) {
        let previous = self.parked_configure_tail.lock().replace(state);
        debug_assert!(
            previous.is_none(),
            "a configure tail parked while another parked tail was pending"
        );
    }

    pub(crate) fn take_parked_configure_tail(
        &self,
    ) -> Option<crate::commands::configure::ConfigureMaintenanceState> {
        self.parked_configure_tail.lock().take()
    }

    pub(crate) fn configure_maintenance_has_capacity(&self) -> bool {
        self.configure_maintenance_jobs.lock().len() < crate::executor::MAINTENANCE_QUEUE_CAP
    }

    pub(crate) fn enqueue_configure_maintenance(
        &self,
        job: ConfigureMaintenanceJob,
    ) -> Result<(), ConfigureMaintenanceJob> {
        let mut jobs = self.configure_maintenance_jobs.lock();
        if jobs.len() >= crate::executor::MAINTENANCE_QUEUE_CAP {
            return Err(job);
        }
        #[cfg(test)]
        record_configure_bind_effect_for_test(
            &job.canonical_cache_root,
            &job.session_id,
            ConfigureBindEffect::Maintenance,
        );
        jobs.push_back(job);
        Ok(())
    }

    pub(crate) fn drain_configure_maintenance(&self) -> Vec<ConfigureMaintenanceJob> {
        self.configure_maintenance_jobs.lock().drain(..).collect()
    }

    /// Peek the memoized artifact key without deriving it. Passive readers
    /// (status snapshots) use this so reporting never spawns a git probe.
    pub fn cached_artifact_cache_key(&self, canonical_root: &Path) -> Option<String> {
        self.artifact_cache_keys.lock().get(canonical_root).cloned()
    }

    /// Return a worktree probe result only while the root's `.git` marker still
    /// matches the marker present when the successful probe was cached.
    pub(crate) fn cached_worktree_bridge(
        &self,
        canonical_root: &Path,
    ) -> Option<(bool, Option<PathBuf>)> {
        #[cfg(test)]
        if self.force_worktree_bridge_reprobe.load(Ordering::SeqCst) {
            return None;
        }

        let signature = git_entry_signature(canonical_root);
        self.worktree_bridge_cache
            .lock()
            .get(canonical_root)
            .filter(|entry| entry.git_entry == signature)
            .map(|entry| (entry.is_worktree_bridge, entry.git_common_dir.clone()))
    }

    /// Cache only successful git worktree probes. Failed probes remain retryable
    /// because a transient process or filesystem error must not become sticky.
    pub(crate) fn cache_worktree_bridge(
        &self,
        canonical_root: &Path,
        is_worktree_bridge: bool,
        git_common_dir: PathBuf,
    ) {
        self.worktree_bridge_cache.lock().insert(
            canonical_root.to_path_buf(),
            WorktreeBridgeCacheEntry {
                git_entry: git_entry_signature(canonical_root),
                is_worktree_bridge,
                git_common_dir: Some(git_common_dir),
            },
        );
    }

    #[cfg(test)]
    pub(crate) fn record_worktree_bridge_probe_spawn_for_test(&self) {
        self.worktree_bridge_probe_spawns
            .fetch_add(1, Ordering::SeqCst);
    }

    #[cfg(test)]
    pub(crate) fn worktree_bridge_probe_spawns_for_test(&self) -> u64 {
        self.worktree_bridge_probe_spawns.load(Ordering::SeqCst)
    }

    #[cfg(test)]
    pub(crate) fn force_worktree_bridge_reprobe_for_test(&self, enabled: bool) {
        self.force_worktree_bridge_reprobe
            .store(enabled, Ordering::SeqCst);
    }

    /// Consume a newly-ready index plane on the query path (`first_query` or a Building wait).
    pub(crate) fn note_index_query(
        &self,
        plane: crate::logging::IndexPlane,
        tool: &str,
        service_ms: u64,
        status: &str,
    ) {
        let root = self
            .canonical_cache_root_opt()
            .or_else(|| self.config().project_root.clone());
        let Some(root) = root else {
            return;
        };
        crate::logging::note_index_query(plane, &root, tool, service_ms, status);
    }

    pub fn memoized_artifact_cache_key(&self, canonical_root: &Path) -> String {
        let mut keys = self.artifact_cache_keys.lock();
        if let Some(key) = keys.get(canonical_root).cloned() {
            return key;
        }
        let key = crate::root_identity::key::artifact_cache_key_for_build(canonical_root);
        self.artifact_cache_key_derivations
            .fetch_add(1, Ordering::SeqCst);
        // Status reports the artifact key used by builds without probing Git.
        // Configure uses a separate memo so it still verifies the root identity.
        //
        // A bound root whose identity is provisional has no key yet: configure
        // published none, and the key derived here comes from Git alone. The
        // settled identity can key differently, because it may keep an older
        // path or repository pin from aft.db or the root's identity record
        // (see `resolve_root_identity_for_bind`). So the caller gets this
        // derivation, but it is not remembered: a passive reader of the
        // cache must not mistake it for the bound key. The check runs under
        // the cache lock, so it pairs with `forget_unsettled_artifact_cache_key`,
        // which a provisional configure calls after publishing its degraded
        // reasons.
        if !self.bound_identity_unsettled(canonical_root) {
            keys.insert(canonical_root.to_path_buf(), key.clone());
        }
        key
    }

    /// The artifact key for `canonical_root`, or `None` when it is this
    /// context's bound root and its last bind left the identity provisional.
    ///
    /// A provisional root has no key yet, and deriving one asks Git again on
    /// every call: nothing caches a failed Git answer, so a root whose Git
    /// identity is unreadable (for example a dubious-ownership refusal) costs
    /// three Git attempts and their retry sleeps each time. Readers that poll,
    /// such as standing-root maintenance, use this instead of
    /// [`Self::memoized_artifact_cache_key`] and handle the provisional root
    /// without a key.
    pub(crate) fn settled_artifact_cache_key(&self, canonical_root: &Path) -> Option<String> {
        if self.bound_identity_unsettled(canonical_root) {
            return None;
        }
        Some(self.memoized_artifact_cache_key(canonical_root))
    }

    /// True when `root` is this context's bound root and its last bind left
    /// the identity provisional.
    fn bound_identity_unsettled(&self, root: &Path) -> bool {
        if !self.artifact_identity_provisional() {
            return false;
        }
        self.canonical_cache_root_opt().as_deref() == Some(root)
            || self.config().project_root.as_deref() == Some(root)
    }

    /// Drop any key cached for a root whose bind just left the identity
    /// provisional. A key derived while the configure was still publishing
    /// (before its degraded reasons said provisional) may have been cached by
    /// a reader on another thread, such as standing-root maintenance.
    pub(crate) fn forget_unsettled_artifact_cache_key(
        &self,
        canonical_root: &Path,
        raw_root: &Path,
    ) {
        let mut keys = self.artifact_cache_keys.lock();
        keys.remove(canonical_root);
        keys.remove(raw_root);
    }

    pub(crate) fn memoized_readonly_artifact_cache_key(
        &self,
        canonical_root: &Path,
        storage: &Path,
    ) -> Result<String, crate::root_identity::key::ArtifactCacheKeyProbeError> {
        // Readers name the root they open artifacts from. A `storage.cache_dir`
        // cache root resolves to its durable owner here, so this memo entry and
        // the identity record it checks are the ones configure uses for that
        // durable root.
        let storage: &Path = &crate::root_cache::identity_storage_root(storage);
        self.memoized_readonly_artifact_cache_key_with(canonical_root, storage, || {
            crate::root_identity::key::artifact_cache_key_read_only(canonical_root, storage)
        })
    }

    fn memoized_readonly_artifact_cache_key_with(
        &self,
        canonical_root: &Path,
        storage: &Path,
        derive: impl FnOnce() -> Result<String, crate::root_identity::key::ArtifactCacheKeyProbeError>,
    ) -> Result<String, crate::root_identity::key::ArtifactCacheKeyProbeError> {
        let cache_key = (canonical_root.to_path_buf(), storage.to_path_buf());
        let before = crate::root_identity::record_stamp(canonical_root, storage);
        if let Ok(record) = &before {
            let keys = self.readonly_artifact_cache_keys.lock();
            if let Some(entry) = keys.get(&cache_key).filter(|entry| entry.record == *record) {
                return Ok(entry.key.clone());
            }
        }
        // Git and legacy database probes must not hold the context-wide mutex:
        // one slow root must not delay readers of unrelated roots.
        let key = derive()?;
        self.artifact_cache_key_derivations
            .fetch_add(1, Ordering::SeqCst);
        // Only remember a derivation whose record stayed stable. Unreadable
        // metadata or a concurrent configure leaves this lookup uncached.
        if let (Ok(before), Ok(after)) = (
            before,
            crate::root_identity::record_stamp(canonical_root, storage),
        ) {
            if before == after {
                self.readonly_artifact_cache_keys.lock().insert(
                    cache_key,
                    ReadonlyArtifactCacheKey {
                        record: after,
                        key: key.clone(),
                    },
                );
            }
        }
        Ok(key)
    }

    /// `Ok(None)` means the root's identity is provisional for this bind; see
    /// [`crate::root_identity::key::artifact_cache_key_for_bind_profiled`].
    pub fn memoized_artifact_cache_key_for_configure(
        &self,
        raw_root: &Path,
        canonical_root: &Path,
        storage_root: &Path,
        git_common_dir: Option<&Path>,
    ) -> Result<Option<String>, crate::root_identity::key::ArtifactCacheKeyProbeError> {
        self.memoized_artifact_cache_key_for_configure_profiled(
            raw_root,
            canonical_root,
            storage_root,
            git_common_dir,
            &mut crate::root_identity::key::ArtifactCacheKeyTimings::default(),
        )
    }

    pub(crate) fn memoized_artifact_cache_key_for_configure_profiled(
        &self,
        raw_root: &Path,
        canonical_root: &Path,
        storage_root: &Path,
        git_common_dir: Option<&Path>,
        timings: &mut crate::root_identity::key::ArtifactCacheKeyTimings,
    ) -> Result<Option<String>, crate::root_identity::key::ArtifactCacheKeyProbeError> {
        {
            let waiting = Instant::now();
            let keys = self.configure_artifact_cache_keys.lock();
            timings.context_memo_wait += waiting.elapsed();
            let storage = storage_root.to_path_buf();
            if let Some(key) = keys
                .get(&(storage.clone(), canonical_root.to_path_buf()))
                .or_else(|| keys.get(&(storage, raw_root.to_path_buf())))
                .cloned()
            {
                timings.context_cache_hit = true;
                // The root-only cache feeds artifact loads and the call-graph
                // directory. After the storage root changes and changes back,
                // it may still hold the other root's key, so refresh it on a
                // hit too. The lock order matches the miss path below.
                let mut current = self.artifact_cache_keys.lock();
                current.insert(canonical_root.to_path_buf(), key.clone());
                current.insert(raw_root.to_path_buf(), key.clone());
                return Ok(Some(key));
            }
        }

        // A provisional key is never memoized, so the next bind resolves the
        // identity again and can finalize it.
        let Some(key) = crate::root_identity::key::artifact_cache_key_for_bind_profiled(
            canonical_root,
            raw_root,
            storage_root,
            git_common_dir,
            timings,
        )?
        else {
            return Ok(None);
        };
        self.artifact_cache_key_derivations
            .fetch_add(1, Ordering::SeqCst);
        let waiting = Instant::now();
        let mut verified = self.configure_artifact_cache_keys.lock();
        verified.insert(
            (storage_root.to_path_buf(), canonical_root.to_path_buf()),
            key.clone(),
        );
        verified.insert(
            (storage_root.to_path_buf(), raw_root.to_path_buf()),
            key.clone(),
        );
        let mut keys = self.artifact_cache_keys.lock();
        timings.context_memo_wait += waiting.elapsed();
        keys.insert(canonical_root.to_path_buf(), key.clone());
        keys.insert(raw_root.to_path_buf(), key.clone());
        Ok(Some(key))
    }

    #[cfg(test)]
    pub fn artifact_cache_key_derivation_count_for_test(&self) -> u64 {
        self.artifact_cache_key_derivations.load(Ordering::SeqCst)
    }

    pub fn configure_generation(&self) -> u64 {
        self.configure_generation.load(Ordering::SeqCst)
    }

    pub fn configure_generation_flag(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.configure_generation)
    }

    pub(crate) fn configure_content_generation(&self) -> u64 {
        self.configure_content_generation.load(Ordering::SeqCst)
    }

    pub(crate) fn begin_configure_ack_phase(&self, phase: &'static str) {
        let now = Instant::now();
        let mut timing = self.configure_phase_timing.lock();
        timing.finish_step(now);
        let steps = std::mem::take(&mut timing.steps);
        if phase == "config_resolve" {
            timing.completed.clear();
        } else if timing.phase != "idle" && timing.phase != "ack_ready" {
            let previous = timing.phase;
            let elapsed = now.saturating_duration_since(timing.started_at);
            timing.completed.push((previous, elapsed, steps));
        }
        timing.phase = phase;
        timing.started_at = now;
    }

    /// Starts a named step inside the current configure phase, ending the
    /// previous step. The phase's entry in the timing line then lists its
    /// steps, so a slow phase names the step that took the time.
    pub(crate) fn begin_configure_ack_step(&self, step: &'static str) {
        let now = Instant::now();
        let mut timing = self.configure_phase_timing.lock();
        timing.finish_step(now);
        timing.step = Some((step, now));
    }

    pub(crate) fn configure_ack_phase_snapshot(&self) -> String {
        let timing = self.configure_phase_timing.lock();
        let mut parts = timing
            .completed
            .iter()
            .map(|(phase, elapsed, steps)| {
                format_configure_phase(phase, *elapsed, steps.iter().copied())
            })
            .collect::<Vec<_>>();
        // The running step is shown with its time so far, so a breadcrumb
        // taken while a configure is stuck names the step it is stuck in.
        let running_step = timing
            .step
            .map(|(step, started_at)| (step, started_at.elapsed()));
        parts.push(format_configure_phase(
            timing.phase,
            timing.started_at.elapsed(),
            timing.steps.iter().copied().chain(running_step),
        ));
        parts.join(",")
    }

    /// Record the configure-tail stage that starts now, or `None` once the
    /// unit has finished.
    pub(crate) fn note_configure_tail_stage(&self, stage: Option<&'static str>) {
        *self.configure_tail_stage.lock() = stage.map(|stage| (stage, Instant::now()));
    }

    /// `stage=<name>,elapsed_ms=<ms>` for the configure-tail unit running now.
    pub(crate) fn configure_tail_stage_snapshot(&self) -> Option<String> {
        self.configure_tail_stage.lock().map(|(stage, started_at)| {
            format!(
                "stage={stage},elapsed_ms={}",
                started_at.elapsed().as_millis()
            )
        })
    }

    pub fn advance_semantic_fingerprint_generation(&self) -> u64 {
        self.semantic_fingerprint_generation
            .fetch_add(1, Ordering::SeqCst)
            .wrapping_add(1)
    }

    pub fn semantic_fingerprint_generation(&self) -> u64 {
        self.semantic_fingerprint_generation.load(Ordering::SeqCst)
    }

    pub fn semantic_fingerprint_generation_flag(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.semantic_fingerprint_generation)
    }

    /// Invalidate an in-flight semantic builder when its corpus inputs change.
    /// This is intentionally independent from the broad configure generation so
    /// unrelated configuration changes can adopt a costly live embedding build.
    pub(crate) fn advance_semantic_build_epoch(&self) -> u64 {
        self.semantic_build_epoch
            .fetch_add(1, Ordering::SeqCst)
            .wrapping_add(1)
    }

    pub fn configure_warnings_sender(
        &self,
    ) -> crossbeam_channel::Sender<(u64, ConfigureWarningsFrame)> {
        self.configure_warnings_tx.clone()
    }

    pub fn drain_configure_warnings(&self) -> Vec<(u64, ConfigureWarningsFrame)> {
        let mut warnings = Vec::new();
        while let Ok(warning) = self.configure_warnings_rx.try_recv() {
            warnings.push(warning);
        }
        warnings
    }

    pub fn bash_background(&self) -> &BgTaskRegistry {
        &self.bash_background
    }

    #[cfg(unix)]
    pub(crate) fn escalation_grants(
        &self,
    ) -> &parking_lot::Mutex<crate::sandbox_spawn::EscalationGrantStore> {
        &self.escalation_grants
    }

    pub fn drain_bg_completions(&self) -> Vec<BgCompletion> {
        self.bash_background.drain_completions()
    }

    /// Access the language provider.
    pub fn provider(&self) -> &dyn LanguageProvider {
        self.provider.as_ref()
    }

    /// Access the backup store.
    pub fn backup(&self) -> &parking_lot::Mutex<BackupStore> {
        &self.backup
    }

    /// Route teardown must not wait for a request holding the backup mutex.
    pub(crate) fn release_backup_session_in_background(
        self: &Arc<Self>,
        session: String,
    ) -> std::thread::JoinHandle<()> {
        let ctx = Arc::clone(self);
        std::thread::spawn(move || {
            ctx.backup.lock().release_session_memory(&session);
        })
    }

    /// Session-scoped hashline bindings installed by successful configure calls.
    pub fn hashline_bindings(&self) -> &crate::hashline::integration::BindingRegistry {
        &self.hashline_bindings
    }

    /// Access the checkpoint store.
    pub fn checkpoint(&self) -> &parking_lot::Mutex<CheckpointStore> {
        &self.checkpoint
    }

    /// Select the checkpoint store's durable namespace without waiting on
    /// anything: no store mutex, no file lock, no I/O. The store applies it at
    /// the start of its next operation; moving checkpoints saved before
    /// configure happens then or in deferred maintenance.
    pub fn request_checkpoint_namespace(
        &self,
        storage_dir: PathBuf,
        harness: crate::harness::Harness,
    ) {
        self.checkpoint_namespace_request
            .request(storage_dir, harness);
    }

    /// Swap in a test-built checkpoint store that answers this context's
    /// namespace requests.
    #[cfg(test)]
    pub(crate) fn replace_checkpoint_store_for_test(&self, mut store: CheckpointStore) {
        store.use_namespace_request(self.checkpoint_namespace_request.clone());
        *self.checkpoint.lock() = store;
    }

    /// Give a newly constructed test context private pre-configure checkpoints.
    /// Integration and binary tests link the library without `cfg(test)`, so
    /// their helpers must opt in explicitly; production construction is unchanged.
    #[doc(hidden)]
    pub fn with_test_checkpoint_store(self) -> Self {
        let mut store = CheckpointStore::unbound_in_for_test(&self.storage_dir());
        store.use_namespace_request(self.checkpoint_namespace_request.clone());
        *self.checkpoint.lock() = store;
        self
    }

    pub fn set_db(&self, conn: Arc<Mutex<TrackedConnection>>) {
        self.app.set_db(conn);
        self.compression_aggregates.clear();
        self.launch_grants_materialized.lock().clear();
        self.launch_grant_state.clear();
    }

    pub fn clear_db(&self) {
        self.app.clear_db();
        self.compression_aggregates.clear();
        self.launch_grants_materialized.lock().clear();
        self.launch_grant_state.clear();
    }

    pub fn db(&self) -> Option<Arc<Mutex<TrackedConnection>>> {
        self.app.db()
    }

    pub(crate) fn launch_grants_materialized(
        &self,
        scope: &crate::db::access_grants::ScopeKey,
    ) -> bool {
        self.launch_grants_materialized.lock().contains(scope)
    }

    pub(crate) fn memoize_launch_grants(&self, scope: crate::db::access_grants::ScopeKey) {
        self.launch_grants_materialized.lock().insert(scope);
    }

    #[cfg(test)]
    pub(crate) fn note_launch_restore_attempt(&self) {
        self.launch_restore_attempts.fetch_add(1, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub(crate) fn launch_restore_attempts(&self) -> usize {
        self.launch_restore_attempts.load(Ordering::Relaxed)
    }

    /// Mark persistence as initializing for a configure that just committed,
    /// and stage its database open in the same step, so a root reported as
    /// initializing always has an open staged or running (never an
    /// initializing state nobody will finish). A new open epoch keeps any open
    /// still running for an older configure from publishing over this one;
    /// taking the open mutex orders this against `publish_database_open`.
    ///
    /// A rebind that keeps an open, ready database reattaches any detached
    /// root-local pools without marking persistence as initializing, which
    /// would refuse bash and edits until another open ran. A failed or busy
    /// database is always retried.
    pub(crate) fn begin_database_runtime(
        &self,
        canonical_cache_root: PathBuf,
        storage_root: PathBuf,
        session_id: String,
    ) {
        let mut slot = self.database_open.lock();
        let target = (canonical_cache_root, storage_root);
        if slot.ready_for.as_ref() == Some(&target) && self.attach_ready_database_pools(&slot) {
            return;
        }
        slot.epoch = slot.epoch.wrapping_add(1);
        slot.committed_at = Instant::now();
        let (canonical_cache_root, storage_root) = target;
        slot.pending = Some(crate::database_open::DatabaseOpenRequest {
            epoch: slot.epoch,
            canonical_cache_root,
            storage_root,
            session_id,
            committed_at: slot.committed_at,
            dispatched_at: None,
        });
        self.database_runtime_state.store(1, Ordering::Release);
    }

    /// The caller holds the database-open slot so a newer configure or open
    /// cannot replace the ready target between checking and attaching its pool.
    fn attach_ready_database_pools(&self, slot: &crate::database_open::DatabaseOpenSlot) -> bool {
        if self.database_runtime_state.load(Ordering::Acquire) != 2
            || slot.running
            || slot.pending.is_some()
        {
            return false;
        }
        let Some((_, storage_root)) = slot.ready_for.as_ref() else {
            return false;
        };
        let Some(shared) = self.app.db_for_path(&storage_root.join("aft.db")) else {
            return false;
        };
        {
            let backup = self.backup.lock();
            if !backup.uses_db_pool(&shared) {
                backup.set_db_pool(shared.clone());
            }
        }
        if !self.bash_background.uses_db_pool(&shared) {
            self.bash_background.set_db_pool(shared);
        }
        true
    }

    /// Mark persistence as initializing with nothing staged, for tests of the
    /// readiness wait itself that must control when the state changes.
    #[cfg(test)]
    pub(crate) fn mark_database_runtime_initializing_for_test(&self) {
        let mut slot = self.database_open.lock();
        slot.epoch = slot.epoch.wrapping_add(1);
        slot.pending = None;
        self.database_runtime_state.store(1, Ordering::Release);
    }

    /// Record that the staged open was handed to the database-open thread
    /// (after a bind reply, or by a tool that found the root initializing).
    /// False when nothing is staged.
    pub(crate) fn mark_database_open_dispatched(&self) -> bool {
        let mut slot = self.database_open.lock();
        match slot.pending.as_mut() {
            Some(request) => {
                request.dispatched_at.get_or_insert_with(Instant::now);
                true
            }
            None => false,
        }
    }

    pub(crate) fn database_open_epoch(&self) -> u64 {
        self.database_open.lock().epoch
    }

    #[cfg(test)]
    pub(crate) fn note_database_open_attempt_for_test(&self) {
        self.database_open_attempts.fetch_add(1, Ordering::AcqRel);
    }

    pub(crate) fn database_open_slot(
        &self,
    ) -> (
        &parking_lot::Mutex<crate::database_open::DatabaseOpenSlot>,
        &parking_lot::Condvar,
    ) {
        (&self.database_open, &self.database_open_idle)
    }

    /// Run `publish` (install or clear the database pools and publish the
    /// readiness outcome) only if no configure committed since the open for
    /// `epoch` began. The open runs outside the actor's write gate, so without
    /// this an open for an older configure could finish after a newer
    /// configure marked persistence as initializing and report its database
    /// as ready. Returns whether `publish` ran.
    pub(crate) fn publish_database_open(
        &self,
        epoch: u64,
        publish: impl FnOnce(&mut crate::database_open::DatabaseOpenSlot),
    ) -> bool {
        let mut slot = self.database_open.lock();
        if slot.epoch != epoch {
            return false;
        }
        publish(&mut slot);
        drop(slot);
        // Database recovery can be the only event on a refused route. Queue its
        // current status after releasing the open slot. Transport roots register
        // their Arc; do not build a synchronous snapshot here for library callers
        // that may still hold the process-shared connection or App slot.
        let context = self
            .status_emitter
            .context
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if context.strong_count() > 0 {
            self.status_emitter.signal_context(context);
        }
        true
    }

    pub(crate) fn finish_database_runtime(&self, result: Result<(), String>) {
        match result {
            Ok(()) => {
                *self.database_runtime_failure.lock() = DatabaseRuntimeFailure::default();
                self.database_runtime_state.store(2, Ordering::Release);
                self.database_runtime_changed.notify_waiters();
            }
            Err(error) => self.finish_database_runtime_error(error, false),
        }
    }

    pub(crate) fn finish_database_runtime_error(&self, error: String, busy: bool) {
        let mut failure = self.database_runtime_failure.lock();
        failure.record(error, busy);
        self.database_runtime_state
            .store(if busy { 4 } else { 3 }, Ordering::Release);
        drop(failure);
        self.database_runtime_changed.notify_waiters();
    }

    pub fn claim_database_runtime_retry(&self, command: &str) -> bool {
        if !crate::persistence_gate::requires_database(command) {
            return false;
        }
        if self.database_runtime_state.load(Ordering::Acquire) == 3 {
            // Concurrent calls must neither queue on a retry nor start their
            // own open. Only the claimant changes failed to initializing.
            let Some(failure) = self.database_runtime_failure.try_lock() else {
                return false;
            };
            if failure.retry_at.is_none_or(|at| Instant::now() < at) {
                return false;
            }
            return self
                .database_runtime_state
                .compare_exchange(3, 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok();
        }
        self.database_runtime_state
            .compare_exchange(5, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub fn retry_database_runtime(&self) {
        let Some(root) = self.canonical_cache_root_opt() else {
            let error = "Cannot retry project persistence: configured database root is missing";
            log::warn!("{error}");
            self.finish_database_runtime_error(error.into(), false);
            return;
        };
        let storage = crate::storage_path::storage_dir(self.config().storage_dir.as_deref());
        if let Err(error) = crate::reader_floor::refresh_refused(&storage) {
            self.finish_database_runtime_error(error.to_string(), false);
            return;
        }
        crate::commands::configure::configure_database_runtime_with_mode(
            self,
            &root,
            &storage,
            crate::db::OpenMode::SingleAttempt,
        );
    }

    pub(crate) fn database_runtime_pending(&self, command: &str) -> bool {
        // Status can read finished task bundles if the store fails, but must
        // wait for an opening store to recover running tasks after a restart.
        (crate::persistence_gate::requires_database(command)
            || command.strip_prefix("aft_").unwrap_or(command) == "bash_status")
            && self.database_runtime_state.load(Ordering::Acquire) == 1
    }

    /// Wait outside executor admission. Register before checking state so an
    /// open completing between the check and await cannot lose its notification.
    pub(crate) async fn wait_for_database_runtime(&self, command: &str, deadline: Instant) {
        self.wait_for_database_while(deadline, || self.database_runtime_pending(command))
            .await;
    }

    /// Wait, up to `deadline`, until persistence is no longer initializing.
    /// Unlike `wait_for_database_runtime`, this does not depend on whether a
    /// particular command needs the database. Afterwards persistence may be
    /// open, failed, unconfigured, or (on timeout) still initializing.
    pub(crate) async fn wait_for_database_settled(&self, deadline: Instant) {
        self.wait_for_database_while(deadline, || {
            self.database_runtime_state.load(Ordering::Acquire) == 1
        })
        .await;
    }

    async fn wait_for_database_while(&self, deadline: Instant, pending: impl Fn() -> bool) {
        loop {
            let changed = self.database_runtime_changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if !pending() {
                return;
            }
            if tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), changed)
                .await
                .is_err()
            {
                return;
            }
        }
    }

    /// True until this context's first configure stages a database open. Such
    /// a context (a library or test context) has never had aft.db open.
    pub(crate) fn database_runtime_unconfigured(&self) -> bool {
        self.database_runtime_state.load(Ordering::Acquire) == 0
    }

    /// True while a committed configure's database open has not finished.
    pub(crate) fn database_runtime_initializing(&self) -> bool {
        self.database_runtime_state.load(Ordering::Acquire) == 1
    }

    /// Wait outside executor admission until the database open in progress
    /// finishes or `deadline` passes, whatever command is waiting.
    pub(crate) async fn wait_for_database_open(&self, deadline: Instant) {
        loop {
            let changed = self.database_runtime_changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if !self.database_runtime_initializing() {
                return;
            }
            if tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), changed)
                .await
                .is_err()
            {
                return;
            }
        }
    }

    /// The remembered confinement of `key`: `Some(true)` once known confined
    /// (which always wins), `Some(false)` once read as unconfined and not
    /// confined since, `None` when only aft.db can answer.
    pub(crate) fn policy_key_confinement(
        &self,
        key: &crate::db::remote_exec::PolicyKey,
    ) -> Option<bool> {
        // Never waits for the database handle slot, which an open holds while
        // SQLite opens: the subc frame loop asks this. A busy slot only means
        // a remembered unconfined answer cannot be checked against it now.
        let db = self.app.try_db().flatten();
        let memo = self.policy_confinement.lock();
        if memo.confined.contains(key) {
            return Some(true);
        }
        let same_db = match (&db, memo.unconfined_db.as_ref().and_then(|db| db.upgrade())) {
            (Some(current), Some(read_from)) => Arc::ptr_eq(current, &read_from),
            _ => false,
        };
        (same_db && memo.unconfined.contains(key)).then_some(false)
    }

    /// Mark `key` confined. Call it before the transaction that saves a
    /// confining policy commits, so no call can use an older "unconfined"
    /// answer once the fence is saved. Every remembered "unconfined" answer
    /// is dropped too, not only this key's: a key with no row for its own
    /// epoch reads an older epoch's row (see `db::remote_exec::confined`), so
    /// fencing one epoch can confine another key of the same worker.
    pub(crate) fn remember_confined_policy_key(&self, key: crate::db::remote_exec::PolicyKey) {
        let mut memo = self.policy_confinement.lock();
        memo.confined.insert(key);
        memo.unconfined.clear();
        memo.generation = memo.generation.wrapping_add(1);
    }

    /// The generation to pass to [`Self::remember_unconfined_policy_key`]:
    /// take it before reading a key's policy from aft.db.
    pub(crate) fn policy_confinement_generation(&self) -> u64 {
        self.policy_confinement.lock().generation
    }

    /// Advance the saved-policy generation and clear cached unconfined answers
    /// before saving catalog params. Candidate comparisons retain this generation
    /// so a save invalidates their older answers. The caller holds aft.db's lock
    /// until the save finishes, so readers cannot observe a pre-save row under
    /// the new generation.
    pub(crate) fn invalidate_saved_policy_reads(&self) {
        let mut memo = self.policy_confinement.lock();
        memo.unconfined.clear();
        memo.generation = memo.generation.wrapping_add(1);
    }

    /// Remember that the aft.db handle `db` said `key` is unconfined, unless a
    /// key was marked confined since `generation` was taken: that read may
    /// predate the fence, so it is not kept and the next call reads again.
    pub(crate) fn remember_unconfined_policy_key(
        &self,
        key: crate::db::remote_exec::PolicyKey,
        generation: u64,
        db: &Arc<Mutex<TrackedConnection>>,
    ) {
        let mut memo = self.policy_confinement.lock();
        if memo.generation != generation || memo.confined.contains(&key) {
            return;
        }
        let same_db = memo
            .unconfined_db
            .as_ref()
            .and_then(|db| db.upgrade())
            .is_some_and(|read_from| Arc::ptr_eq(db, &read_from));
        if !same_db {
            memo.unconfined.clear();
            memo.unconfined_db = Some(Arc::downgrade(db));
        }
        memo.unconfined.insert(key);
    }

    /// Count one read of a policy key's confinement from aft.db.
    pub(crate) fn note_policy_confinement_read(&self) {
        #[cfg(test)]
        {
            self.policy_confinement.lock().store_reads += 1;
        }
    }

    #[cfg(test)]
    pub(crate) fn policy_confinement_reads_for_test(&self) -> usize {
        self.policy_confinement.lock().store_reads
    }

    /// Run `hook` once, after the next policy read from aft.db and before its
    /// answer is remembered.
    #[cfg(test)]
    pub(crate) fn after_next_policy_read_for_test(
        &self,
        hook: impl FnOnce(&AppContext) + Send + 'static,
    ) {
        self.policy_confinement.lock().after_read_for_test = Some(Box::new(hook));
    }

    #[cfg(test)]
    pub(crate) fn run_after_policy_read_hook_for_test(&self) {
        // Taken out first: the hook may save a policy, which locks the memo.
        let hook = self.policy_confinement.lock().after_read_for_test.take();
        if let Some(hook) = hook {
            hook(self);
        }
    }

    #[cfg(test)]
    pub(crate) fn after_next_policy_memory_miss_for_test(
        &self,
        hook: impl FnOnce() + Send + 'static,
    ) {
        self.policy_confinement.lock().after_memory_miss_for_test = Some(Box::new(hook));
    }

    #[cfg(test)]
    pub(crate) fn run_after_policy_memory_miss_hook_for_test(&self) {
        let hook = self
            .policy_confinement
            .lock()
            .after_memory_miss_for_test
            .take();
        if let Some(hook) = hook {
            hook();
        }
    }

    pub(crate) fn database_runtime_failed(&self) -> bool {
        matches!(
            self.database_runtime_state.load(Ordering::Acquire),
            3 | 4 | 5
        )
    }

    pub(crate) fn database_runtime_unavailable_reason(&self) -> Option<String> {
        match self.database_runtime_state.load(Ordering::Acquire) {
            1 => Some("project persistence is still initializing".into()),
            3 | 4 | 5 => Some(
                self.database_runtime_failure
                    .lock()
                    .error
                    .clone()
                    .unwrap_or_else(|| "project persistence could not be opened".into()),
            ),
            _ => None,
        }
    }

    /// Persistence-dependent commands must not silently use a JSON fallback.
    /// Read-only commands and unconfigured library contexts remain available.
    pub fn database_runtime_refusal(
        &self,
        request_id: &str,
        command: &str,
    ) -> Option<crate::protocol::Response> {
        if !crate::persistence_gate::requires_database(command) {
            return None;
        }
        // Some library callers enter this gate without transport admission.
        // They get the same bounded reopen as standalone and daemon calls,
        // never the bind's longer deferred initialization retry loop.
        if matches!(self.database_runtime_state.load(Ordering::Acquire), 3 | 5)
            && self.claim_database_runtime_retry(command)
        {
            self.retry_database_runtime();
        }
        let failure;
        let (code, message, retryable) = match self.database_runtime_state.load(Ordering::Acquire) {
            1 => (
                "database_initializing",
                "Project persistence is still initializing after route bind; retry the tool shortly. No tool operation was performed.",
                true,
            ),
            4 | 5 => {
                // Report exhaustion once before permitting another bounded attempt.
                // Otherwise the waiter that exhausted the budget would reopen again.
                let _ = self.database_runtime_state.compare_exchange(4, 5, Ordering::AcqRel, Ordering::Acquire);
                failure = format!(
                    "Project persistence is busy after bounded initialization retries: {}. Retry the tool shortly; no rebind is needed. No tool operation was performed.",
                    self.database_runtime_failure.lock().error.as_deref().unwrap_or("database busy")
                );
                ("database_unavailable", failure.as_str(), true)
            },
            3 => {
                let state = self.database_runtime_failure.lock();
                let remaining = state.retry_at.map_or(Duration::ZERO, |at| {
                    at.saturating_duration_since(Instant::now())
                });
                failure = format!(
                    "Project persistence could not be opened: {}. Read-only tools still work. AFT retries on the next persistence-requiring call after {} seconds; no rebind is needed. No tool operation was performed.",
                    state.error.as_deref().unwrap_or("unknown database error"),
                    remaining.as_secs() + u64::from(remaining.subsec_nanos() != 0),
                );
                ("database_unavailable", failure.as_str(), true)
            },
            _ => return None,
        };
        Some(crate::protocol::Response::error_with_data(
            request_id,
            code,
            message,
            serde_json::json!({ "retryable": retryable }),
        ))
    }

    pub(crate) fn compression_aggregate_cache(
        &self,
    ) -> &crate::db::compression_events::CompressionAggregateCache {
        self.compression_aggregates.as_ref()
    }

    pub fn note_request(&self) {
        *self.last_request_at.lock() = Instant::now();
    }

    pub fn last_request_at(&self) -> Instant {
        *self.last_request_at.lock()
    }

    /// A non-waiting snapshot for the frame-loop reaper. If the request
    /// timestamp or configuration lock is contended, defer release.
    pub(crate) fn try_views_idle_request(&self) -> Option<(Instant, u32)> {
        let config = try_read_unpoisoned(&self.config)?;
        self.last_request_at
            .try_lock()
            .map(|stamp| (*stamp, config.idle.checkout_hours))
    }

    #[cfg(test)]
    pub fn set_last_request_at_for_test(&self, at: Instant) {
        *self.last_request_at.lock() = at;
    }

    /// Return whether a tool is available to the active agent session.
    pub fn tool_enabled(&self, tool: &str) -> bool {
        !self.config().disabled_tools.iter().any(|name| name == tool)
    }

    /// Access an owned configuration snapshot.
    pub fn config(&self) -> Arc<Config> {
        assert_config_write_lock_not_held();
        if let Some(pinned) = pinned_config_for(self.config_pin_key()) {
            return pinned;
        }
        self.config_unpinned()
    }

    /// The published configuration, ignoring any snapshot the calling request
    /// pinned. Read-modify-write callers use this so they never republish an
    /// older snapshot over a newer one.
    pub fn config_unpinned(&self) -> Arc<Config> {
        assert_config_write_lock_not_held();
        let guard = match self.config.read() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        Arc::clone(&*guard)
    }

    /// Atomically publish a fully-built configuration snapshot.
    ///
    /// Which language servers apply to the project (the diagnostics
    /// authority behind the status bar) is recomputed on a background
    /// thread, never here: that walks the whole project tree and then waits
    /// for the language-server manager lock, and configure publishes on the
    /// bind path, where a large or busy checkout made route binds miss their
    /// deadline. Until the walk finishes, the status bar reports diagnostics
    /// as pending for a new root, and keeps the previous result on a rebind
    /// of the same root.
    pub fn set_config(&self, config: Config) {
        let previous_root = self.config_unpinned().project_root.clone();
        // Configure publishes this way, and it runs inside a pinned request:
        // it must see its own publication for the rest of that request.
        let _ = self.publish_config(|_| Some(config), true);
        let config = self.config();
        let root_changed = previous_root.as_deref() != config.project_root.as_deref();
        self.refresh_project_applicability(config, root_changed);
        self.note_request();
    }

    /// Refreshes the project snapshot on one worker. Batches arriving during a
    /// walk replace one pending request instead of creating waiting threads.
    fn refresh_project_applicability(&self, config: Arc<Config>, root_changed: bool) {
        // Applicability only reads project files and LSP settings. A detached
        // walk must not keep storage locks and pinned directories alive after
        // the context's last storage-using owner has gone away.
        let mut walk_config = (*config).clone();
        walk_config.cache_lease = None;
        walk_config.storage_lease = None;
        let config = Arc::new(walk_config);
        let progress = Arc::clone(&self.project_applicability_progress);
        let mut refresh = progress.refresh.lock();
        let epoch = self
            .project_applicability_epoch
            .fetch_add(1, Ordering::SeqCst)
            .wrapping_add(1);
        let root_changed = root_changed
            || refresh
                .pending
                .as_ref()
                .is_some_and(|request| request.root_changed);
        refresh.pending = Some(ApplicabilityRefresh {
            config,
            epoch,
            root_changed,
        });
        if refresh.running {
            return;
        }
        if refresh
            .pending
            .as_ref()
            .is_some_and(|request| request.config.project_root.is_none())
        {
            refresh.pending = None;
            drop(refresh);
            progress.settle(epoch);
            return;
        }
        refresh.running = true;
        drop(refresh);
        let epochs = Arc::clone(&self.project_applicability_epoch);
        let manager = Arc::clone(&self.lsp_manager);
        let status_context = self
            .status_emitter
            .context
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        #[cfg(test)]
        progress.refresh_spawns.fetch_add(1, Ordering::SeqCst);
        let worker_progress = Arc::clone(&progress);
        let spawned = std::thread::Builder::new()
            .name("aft-lsp-applicability".to_string())
            .spawn(move || {
                // A panic must release the worker reservation as well as waiters.
                struct WorkerGuard(Arc<ProjectApplicabilityProgress>, bool);
                impl Drop for WorkerGuard {
                    fn drop(&mut self) {
                        if self.1 {
                            let pending = {
                                let mut refresh = self.0.refresh.lock();
                                refresh.running = false;
                                refresh.pending.take()
                            };
                            if let Some(request) = pending {
                                self.0.settle(request.epoch);
                            }
                        }
                    }
                }
                struct Settled(Arc<ProjectApplicabilityProgress>, u64);
                impl Drop for Settled {
                    fn drop(&mut self) {
                        self.0.settle(self.1);
                    }
                }
                let progress = worker_progress;
                let mut worker = WorkerGuard(Arc::clone(&progress), true);
                loop {
                    let request = {
                        let mut refresh = progress.refresh.lock();
                        match refresh.pending.take() {
                            Some(request) => request,
                            None => {
                                refresh.running = false;
                                worker.1 = false;
                                break;
                            }
                        }
                    };
                    let ApplicabilityRefresh {
                        config,
                        epoch,
                        root_changed,
                    } = request;
                    let settled = Settled(Arc::clone(&progress), epoch);
                    // Even a superseded root change must clear the previous root's
                    // snapshot: a later watcher-only request may now own publication.
                    if root_changed {
                        manager.lock().clear_project_applicability();
                    }
                    let Some(root) = config.project_root.as_deref() else {
                        continue;
                    };
                    let current = || epochs.load(Ordering::SeqCst) == epoch;
                    progress.pause_while_held();
                    if !current() {
                        continue;
                    }
                    #[cfg(test)]
                    progress.note_scan_for_test();
                    let walk = match crate::lsp::manager::walk_applicable_area(
                        root, None, &config, None,
                    ) {
                        Ok(walk) => walk,
                        Err(error) => {
                            // Entry-limit failures are logged by the cache once
                            // per retry, not on every lookup during backoff.
                            if !matches!(
                                error,
                                crate::lsp::manager::ApplicabilityResolutionError::EntryLimitReached { .. }
                            ) {
                                crate::slog_warn!(
                                    "language-server applicability walk stopped early: {error:?}"
                                );
                            }
                            if current() {
                                let mut lsp = manager.lock();
                                if current() {
                                    lsp.clear_project_applicability();
                                }
                            }
                            continue;
                        }
                    };
                    {
                        let mut lsp = manager.lock();
                        if !current() {
                            continue;
                        }
                        let snapshot = lsp.classify_applicable_servers(walk, &config);
                        lsp.store_project_applicability(snapshot);
                    }
                    // Waiters need the stored result, not status publication.
                    drop(settled);
                    if let Some(ctx) = status_context.upgrade() {
                        ctx.signal_status_changed();
                    }
                }
            });
        if let Err(error) = spawned {
            let pending = {
                let mut refresh = progress.refresh.lock();
                refresh.running = false;
                refresh.pending.take()
            };
            if let Some(request) = pending {
                progress.settle(request.epoch);
            }
            crate::slog_warn!("could not start the language-server applicability walk: {error}");
        }
    }

    /// Whether the applicability walk started by the latest configuration
    /// publication is still running.
    pub(crate) fn project_applicability_walk_running(&self) -> bool {
        let latest = self.project_applicability_epoch.load(Ordering::SeqCst);
        self.project_applicability_progress
            .settled
            .load(Ordering::SeqCst)
            < latest
    }

    /// Waits, at most `timeout`, until the language-server applicability walk
    /// started by the latest configuration publication has finished. Returns
    /// whether it finished. Callers that turn applicability into a reported
    /// diagnostics state (blocking inspect, tests reading the status bar right
    /// after configure) use it so a walk still running is not reported as
    /// "pending" diagnostics.
    pub fn wait_for_project_applicability(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let progress = &self.project_applicability_progress;
        let mut gate = progress.gate.lock();
        gate.waiters += 1;
        let finished = loop {
            if !self.project_applicability_walk_running() {
                break true;
            }
            if progress.changed.wait_until(&mut gate, deadline).timed_out() {
                break !self.project_applicability_walk_running();
            }
        };
        gate.waiters -= 1;
        finished
    }

    /// Test hook: pause this context's applicability walks before they walk
    /// until the returned guard is dropped. A walk that clears the previous
    /// root's result does so before pausing.
    #[doc(hidden)]
    pub fn hold_project_applicability_walks_for_test(&self) -> ProjectApplicabilityHold {
        self.project_applicability_progress.gate.lock().held = true;
        ProjectApplicabilityHold(Arc::clone(&self.project_applicability_progress))
    }

    /// Test hook: how many callers are inside `wait_for_project_applicability`.
    #[doc(hidden)]
    pub fn project_applicability_waiters_for_test(&self) -> usize {
        self.project_applicability_progress.gate.lock().waiters
    }

    /// Publish `config` only if the published snapshot is still `expected`.
    /// A live config reload builds its snapshot from the one it read, so it
    /// must not overwrite a configure that published in between.
    pub fn publish_config_if_current(&self, expected: &Arc<Config>, config: Config) -> bool {
        self.publish_config_if_current_retaining(expected, config)
            .is_some()
    }

    pub(crate) fn publish_config_if_current_retaining(
        &self,
        expected: &Arc<Config>,
        config: Config,
    ) -> Option<Arc<Config>> {
        self.publish_config(
            |current| Arc::ptr_eq(expected, current).then_some(config),
            false,
        )
    }

    /// Build the next snapshot from the current one and publish it, both
    /// under the configuration write lock, so no other publication can land
    /// between the read and the write. `replace_pin` makes the calling
    /// thread's own request pin follow the publication (configure only).
    fn publish_config(
        &self,
        build: impl FnOnce(&Arc<Config>) -> Option<Config>,
        replace_pin: bool,
    ) -> Option<Arc<Config>> {
        // A publication from inside an `update_config` closure would wait on
        // the write lock this thread already holds.
        assert_config_write_lock_not_held();
        // Keep derived backend, view and watcher state in the same order as snapshots.
        // Acquire before the config lock; effects may read config after the swap.
        let _publication = self.config_publication.lock();
        let (next, project_root_changed) = {
            let mut guard = self
                .config
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let built = {
                let _held = ConfigWriteLockHeld::enter();
                build(&guard)
            };
            let Some(next) = built else {
                return None;
            };
            let mut next = next;
            next.aft_search_registered = next.tool_registered("aft_search");
            #[cfg(test)]
            let next = self.app.isolate_test_config(next);
            if let Some(lease) = next.cache_lease.as_ref() {
                let mut leases = self.cache_leases.lock();
                if !leases.iter().any(|old| old.same_root(lease)) {
                    leases.push(Arc::clone(lease));
                }
            }
            let next = Arc::new(next);
            // Compare the configured spelling, not a normalized equivalent:
            // that spelling is the memo key for containment-root resolution.
            let changed = guard.project_root.as_ref().map(|root| root.as_os_str())
                != next.project_root.as_ref().map(|root| root.as_os_str());
            *guard = Arc::clone(&next);
            (next, changed)
        };
        if replace_pin {
            replace_pinned_config(self.config_pin_key(), &next);
        }
        #[cfg(test)]
        CONFIG_PUBLICATION_PROBE.with(|probe| {
            if let Some(probe) = probe.borrow_mut().take() {
                probe();
            }
        });
        self.reconcile_rerank_backend(&next);
        if project_root_changed {
            self.path_restriction_root_memo.lock().take();
            *self
                .watcher_counters
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = next
                .project_root
                .as_deref()
                .map(watcher_counters_for_root)
                .unwrap_or_else(|| Arc::new(WatcherCounters::default()));
        }
        Some(next)
    }

    /// Pin the published configuration for the rest of the calling request.
    ///
    /// Until the guard drops, `config()` on this thread returns the snapshot
    /// that was published when the request was admitted, so a live config
    /// reload switches over between requests and never in the middle of one.
    /// A nested pin for the same context keeps the outer snapshot.
    pub fn pin_config(&self) -> ConfigPinGuard {
        self.pin_config_to(self.config())
    }

    /// Pin `snapshot` for the rest of the calling scope on this thread.
    ///
    /// Work a request hands to another thread (deferred LSP navigation,
    /// inspect, offloaded validation) captures `ctx.config()` while the
    /// request is being admitted and installs it here on the worker, so the
    /// worker keeps the request's admitted snapshot even if a live config
    /// reload publishes before the worker starts.
    pub fn pin_config_to(&self, snapshot: Arc<Config>) -> ConfigPinGuard {
        let key = self.config_pin_key();
        let pushed = CONFIG_PINS.with(|pins| {
            let mut pins = pins.borrow_mut();
            if pins.iter().any(|(pinned_key, _)| *pinned_key == key) {
                return false;
            }
            pins.push((key, snapshot));
            true
        });
        ConfigPinGuard {
            key: pushed.then_some(key),
            _not_send: std::marker::PhantomData,
        }
    }

    fn config_pin_key(&self) -> usize {
        self.config_pin_identity
    }

    pub fn config_live(&self) -> &crate::config_live::ConfigLiveState {
        &self.config_live
    }

    #[cfg(test)]
    pub(crate) fn path_restriction_root_memo_is_empty_for_test(&self) -> bool {
        self.path_restriction_root_memo.lock().is_none()
    }

    #[cfg(test)]
    pub(crate) fn path_restriction_root_canonicalizations_for_test(&self) -> usize {
        self.path_restriction_root_canonicalizations
            .load(Ordering::SeqCst)
    }

    /// Clone-mutate-publish the current configuration without returning a guard.
    ///
    /// The update runs on the snapshot that is published at that moment,
    /// under the configuration write lock, so it can never overwrite a newer
    /// publication (a live config reload) with values it read earlier. It
    /// must not read the configuration itself. A request pin on the calling
    /// thread keeps its admitted snapshot.
    pub fn update_config(&self, update: impl FnOnce(&mut Config)) {
        let _ = self.publish_config(
            |current| {
                let mut next = current.as_ref().clone();
                update(&mut next);
                Some(next)
            },
            false,
        );
    }

    pub fn force_restrict_guard(&self, req_id: &str) -> ForceRestrictGuard<'_> {
        let mut requests = self.force_restrict_requests.lock();
        *requests.entry(req_id.to_string()).or_insert(0) += 1;
        ForceRestrictGuard {
            ctx: self,
            req_id: req_id.to_string(),
        }
    }

    pub fn with_force_restrict<R>(&self, req_id: &str, f: impl FnOnce() -> R) -> R {
        let _guard = self.force_restrict_guard(req_id);
        f()
    }

    pub fn request_force_restrict(&self, req_id: &str) -> bool {
        self.force_restrict_requests.lock().contains_key(req_id)
    }

    /// Records a transport capability, never an agent-supplied tool argument.
    pub fn record_request_handles_progress(&self, req_id: &str) {
        self.progress_requests.lock().insert(req_id.to_owned());
    }

    pub fn request_handles_progress(&self, req_id: &str) -> bool {
        self.progress_requests.lock().contains(req_id)
    }

    /// Terminal replies retire the capability before the ID can be reused.
    pub fn clear_request_handles_progress(&self, req_id: &str) {
        self.progress_requests.lock().remove(req_id);
    }

    /// The outside-roots rule this request is judged under: the configured
    /// rule, or deny when the request forces it.
    pub(crate) fn request_outside_roots(&self, req_id: &str) -> crate::access_policy::OutsideRoots {
        crate::access_policy::OutsideRoots::for_request(
            &self.config(),
            self.request_force_restrict(req_id),
        )
    }

    fn release_force_restrict(&self, req_id: &str) {
        let mut requests = self.force_restrict_requests.lock();
        match requests.get_mut(req_id) {
            Some(count) if *count > 1 => *count -= 1,
            Some(_) => {
                requests.remove(req_id);
            }
            None => {}
        }
    }

    pub fn set_harness(&self, harness: Harness) {
        self.bash_background.set_harness(harness.clone());
        *self.harness.lock() = Some(harness);
    }

    pub fn harness_opt(&self) -> Option<Harness> {
        self.harness.lock().clone()
    }

    /// Return the harness chosen by configure.
    ///
    /// # Panics
    /// Panics before configure has set a harness. Use [`Self::harness_opt`] when
    /// working with a context that may not yet be configured.
    pub fn harness(&self) -> Harness {
        self.harness_opt()
            .expect("harness set by configure before any tool call")
    }

    pub fn storage_dir(&self) -> PathBuf {
        crate::storage_path::storage_dir(self.config().storage_dir.as_deref())
    }

    pub(crate) fn output_files(&self) -> &crate::output_store::OutputFiles {
        &self.output_files
    }

    pub(crate) fn output_files_arc(&self) -> Arc<crate::output_store::OutputFiles> {
        Arc::clone(&self.output_files)
    }

    pub fn cache_dir(&self) -> PathBuf {
        crate::root_cache::cache_root(&self.config())
    }
    pub fn storage_paths(&self) -> crate::root_cache::StoragePaths {
        crate::root_cache::StoragePaths::for_config(&self.config())
    }

    pub fn cache_unavailable_reason(&self) -> Option<String> {
        self.cache_unavailable_reason_for_root(&crate::root_cache::cache_root(&self.config()))
    }

    /// [`Self::cache_unavailable_reason`] against an already-resolved cache
    /// root. The health rollup resolves each distinct root once per tick and
    /// reuses it here so a many-context tick does not re-resolve the same
    /// root once per context.
    pub(crate) fn cache_unavailable_reason_for_root(&self, cache_dir: &Path) -> Option<String> {
        let reason = crate::root_cache::cache_unavailable_for_root(&self.config(), cache_dir)?;
        if reason.starts_with(crate::root_cache::CACHE_DIR_BUSY)
            && !crate::provisional_identity_retry::cache_retry_pending(self)
        {
            // The folder is still busy, but cancelled or unbound work must not
            // promise an automatic retry to an agent.
            return Some(crate::root_cache::CACHE_DIR_BUSY.to_string());
        }
        Some(reason)
    }

    /// A connect is starting: any deferred cache recovery queued for an
    /// earlier connect is now stale.
    pub(crate) fn note_connect_started(&self) {
        self.connect_generation.fetch_add(1, Ordering::SeqCst);
    }

    /// A connect was refused and the configuration it would have replaced
    /// stays active: give that configuration's retained request the current
    /// connect generation, so later cache recoveries re-run it again. Work
    /// queued before the refused connect keeps the older generation and is
    /// still dropped.
    pub(crate) fn restamp_cache_recovery_request(&self) {
        let generation = self.connect_generation();
        if let Some((_, _, stamped)) = self.cache_recovery.lock().as_mut() {
            *stamped = generation;
        }
    }

    pub(crate) fn connect_generation(&self) -> u64 {
        self.connect_generation.load(Ordering::SeqCst)
    }

    pub(crate) fn retain_cache_recovery_request(&self, request: &crate::protocol::RawRequest) {
        let generation = self.connect_generation();
        *self.cache_recovery.lock() = self
            .config()
            .cache_dir
            .as_ref()
            .map(|_| (request.clone(), None, generation));
    }

    /// The retained connect request, with the connect generation it belongs
    /// to, at most once per minute.
    pub(crate) fn cache_recovery_request_if_due(
        &self,
        now: Instant,
    ) -> Option<(crate::protocol::RawRequest, u64)> {
        let mut recovery = self.cache_recovery.lock();
        let (request, checked, generation) = recovery.as_mut()?;
        if checked.is_some_and(|last| now.saturating_duration_since(last) < Duration::from_secs(60))
        {
            return None;
        }
        *checked = Some(now);
        Some((request.clone(), *generation))
    }

    /// Harness-scoped storage directory. Requires configure to have set a harness;
    /// see [`Self::harness`] for the precondition and [`Self::harness_opt`] to check it.
    pub fn harness_dir(&self) -> PathBuf {
        self.storage_dir().join(self.harness().storage_segment())
    }

    pub fn inspect_dir(&self) -> PathBuf {
        if let Some(root) = self
            .canonical_cache_root_opt()
            .or_else(|| self.config().project_root.clone())
        {
            self.cache_dir()
                .join("inspect")
                .join(crate::path_identity::project_scope_key(&root))
        } else {
            self.cache_dir().join("inspect").join("unconfigured")
        }
    }

    pub fn bash_tasks_dir(&self, session_id: &str) -> PathBuf {
        self.harness_dir()
            .join("bash-tasks")
            .join(hash_session(session_id))
    }

    pub fn backups_dir(&self, session_id: &str, path_hash: &str) -> PathBuf {
        self.harness_dir()
            .join("backups")
            .join(hash_session(session_id))
            .join(path_hash)
    }

    pub fn filters_dir(&self) -> PathBuf {
        self.harness_dir().join("filters")
    }

    /// HOST-GLOBAL — NOT under harness_dir. Read by trust.rs across both harnesses.
    pub fn trust_file(&self) -> PathBuf {
        self.storage_dir().join("trusted-filter-projects.json")
    }

    pub fn set_canonical_cache_root(&self, root: PathBuf) {
        debug_assert!(root.is_absolute());
        let root_changed = {
            let mut current = self.canonical_cache_root.lock();
            let changed = current.as_deref() != Some(root.as_path());
            *current = Some(root);
            changed
        };
        if root_changed {
            let mut todos = self
                .status_bar_todos
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let generation = todos.generation.wrapping_add(1);
            *todos = StatusBarTodos {
                generation,
                ..StatusBarTodos::default()
            };
            self.status_bar_last_emitted.clear();
        }
    }

    pub fn canonical_cache_root(&self) -> PathBuf {
        self.canonical_cache_root
            .lock()
            .clone()
            .expect("canonical_cache_root accessed before handle_configure")
    }

    pub fn canonical_cache_root_opt(&self) -> Option<PathBuf> {
        self.canonical_cache_root.lock().clone()
    }

    /// This root's checkout in the data layer, bound on first use; `None`
    /// when no root is configured, the root is a parent folder of repositories,
    /// or its store cannot be opened.
    pub(crate) fn data_checkout(&self) -> Option<Arc<crate::data_layer::runtime::DataCheckout>> {
        if self.data_checkout_pinned.load(Ordering::Acquire) {
            return self.data_checkout.lock().clone();
        }
        if self.data_layer_withheld().is_some() {
            return None;
        }
        let root = self.canonical_cache_root_opt()?;
        // A parent folder of repositories builds nothing of its own: each
        // child repository has a checkout of its own instead, which this
        // context keeps only while it is bound to that parent folder.
        let parent = crate::parent_search::session_for(self);
        self.data_children
            .keep_only(parent.as_ref().map(|session| session.root()));
        if parent.is_some() {
            self.data_checkout.lock().take();
            return None;
        }
        let planes = self.data_layer_planes();
        let mut slot = self.data_checkout.lock();
        // The index switches take effect at the next bind, as they do for
        // today's indexes; a checkout bound with other planes is replaced.
        if slot.as_ref().is_none_or(|checkout| {
            checkout.root() != root || checkout.checkout().planes() != planes
        }) {
            let executor = self.callgraph_executor.lock().clone();
            *slot = match crate::data_layer::runtime::DataCheckout::open_planes(
                &root,
                &self.storage_dir(),
                executor,
                planes,
            ) {
                Ok(checkout) => {
                    *self.data_bind_failure.lock() = None;
                    // Status pushes follow the data layer's builds and
                    // refreshes. Only a context its transport owns can push.
                    let context = self
                        .status_emitter
                        .context
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .clone();
                    // A TODO count taken while the store was building came
                    // back pending; once the store publishes, take it again
                    // rather than wait for the next edit.
                    let todos_source = Arc::downgrade(&checkout);
                    checkout.on_status_change(Arc::new(move || {
                        if let Some(context) = context.upgrade() {
                            context.signal_status_changed();
                            if todos_source
                                .upgrade()
                                .is_some_and(|checkout| checkout.take_todos_readable())
                            {
                                context.request_tier2_refresh_pull();
                            }
                        }
                    }));
                    Some(checkout)
                }
                Err(error) => {
                    crate::slog_warn!("data layer could not bind {}: {error}", root.display());
                    *self.data_bind_failure.lock() = Some(error);
                    None
                }
            };
        }
        slot.clone()
    }

    /// The search of the repository at `root`, outside this context's root,
    /// on the data layer: the one this context holds, or `bind()`'s, kept
    /// until the context's idle handles are released. A `None` from `bind`
    /// is not kept, so the next query looks again.
    pub(crate) fn external_data_search(
        &self,
        root: &Path,
        bind: impl FnOnce() -> Option<crate::parent_search::external::ExternalSearch>,
    ) -> Option<Arc<crate::parent_search::external::ExternalSearch>> {
        let mut external = self.data_external.lock();
        if let Some(search) = external.get(root) {
            return Some(Arc::clone(search));
        }
        let search = Arc::new(bind()?);
        external.insert(root.to_path_buf(), Arc::clone(&search));
        Some(search)
    }

    /// Makes `checkout` this context's checkout in the data layer, kept
    /// until another is pinned: for the search engine a parent folder's
    /// session runs for one child repository, which answers from the
    /// checkout the session holds for that child (see
    /// `data_layer::parent::search_children`), never one of its own.
    pub(crate) fn pin_data_checkout(
        &self,
        checkout: Arc<crate::data_layer::runtime::DataCheckout>,
    ) {
        let mut slot = self.data_checkout.lock();
        if !slot
            .as_ref()
            .is_some_and(|pinned| Arc::ptr_eq(pinned, &checkout))
        {
            *slot = Some(checkout);
        }
        self.data_checkout_pinned.store(true, Ordering::Release);
    }

    /// This root's checkout in the data layer as status, health and the
    /// memory census see it: never binds one (binding starts a build) and
    /// never waits for a bind in progress. `None` when the data layer is
    /// off.
    pub(crate) fn probe_data_checkout(
        &self,
    ) -> Option<crate::data_layer::surfaces::DataCheckoutProbe> {
        use crate::data_layer::surfaces::DataCheckoutProbe;
        if self.data_checkout_pinned.load(Ordering::Acquire) {
            if let Some(checkout) = self.data_checkout.try_lock().and_then(|slot| slot.clone()) {
                return Some(DataCheckoutProbe::Bound(checkout));
            }
        }
        if let Some(withheld) = self.data_layer_withheld() {
            return Some(DataCheckoutProbe::Withheld(withheld));
        }
        let Some(slot) = self.data_checkout.try_lock() else {
            return Some(DataCheckoutProbe::Binding);
        };
        let root = self.canonical_cache_root_opt();
        if let Some(checkout) = slot
            .as_ref()
            .filter(|checkout| root.as_deref() == Some(checkout.root()))
        {
            return Some(DataCheckoutProbe::Bound(Arc::clone(checkout)));
        }
        drop(slot);
        Some(match self.data_bind_failure.lock().clone() {
            Some(error) => DataCheckoutProbe::BindFailed(error),
            None => DataCheckoutProbe::NotBound,
        })
    }

    /// The data layer's current view of this root for text queries, when it
    /// runs, builds the text index and has published; see
    /// [`Self::data_checkout`]. With the text index switched off, grep and
    /// glob walk the folder instead.
    pub(crate) fn data_view(&self) -> Option<crate::data_layer::checkout::CheckoutView> {
        self.data_checkout()?
            .view()
            .filter(|view| view.planes().text)
    }

    /// The semantic configuration the data layer's meaning search fills and
    /// searches with: `None` when semantic search is switched off
    /// (`indexes.semantic`) or not admitted for this root, as today's index
    /// is not built for it then.
    pub(crate) fn data_meaning_config(&self) -> Option<crate::config::SemanticBackendConfig> {
        let config = self.config();
        if !config.indexes.semantic {
            return None;
        }
        let root = self.canonical_cache_root_opt()?;
        crate::semantic_admission::admission(&root, &config.index.roots)
            .admits()
            .then(|| config.semantic.clone())
    }

    /// Records that the data layer followed the current configure; `false`
    /// when it already had, or no root is configured yet.
    pub(crate) fn note_data_layer_followed_configure(&self) -> bool {
        if self.canonical_cache_root_opt().is_none() {
            return false;
        }
        let generation = self.configure_generation();
        self.data_layer_followed_configure
            .swap(generation, Ordering::AcqRel)
            != generation
    }

    /// Whether the data layer's meaning plane serves this root's semantic
    /// search: the data layer binds this root (it is not withheld, nor a
    /// parent folder of repositories), and semantic search is on and
    /// admitted for it.
    pub(crate) fn data_layer_serves_meaning(&self) -> bool {
        self.data_layer_withheld().is_none()
            && crate::parent_search::session_for(self).is_none()
            && self.data_meaning_config().is_some()
    }

    /// The planes this root's checkout builds: the text index and the call
    /// graph, as `indexes.trigram` and `indexes.callgraph` switch them.
    pub(crate) fn data_layer_planes(&self) -> crate::data_layer::checkout::Planes {
        let config = self.config();
        crate::data_layer::checkout::Planes {
            text: config.indexes.trigram,
            callgraph: config.indexes.callgraph,
        }
    }

    /// Why this root's checkout is never bound in the data layer, as
    /// configure leaves today's indexes off for it: the root is a
    /// filesystem, volume or drive root, or the home folder, or every index
    /// is switched off. Semantic search alone still binds it: the meaning
    /// plane fills from the checkout's file list, which a build publishes
    /// even with the text index and call graph off.
    pub(crate) fn data_layer_withheld(&self) -> Option<crate::data_layer::surfaces::Withheld> {
        use crate::data_layer::surfaces::Withheld;
        let planes = self.data_layer_planes();
        if self.root_too_broad() {
            Some(Withheld::TooBroad)
        } else if self.is_home_root() {
            Some(Withheld::Home)
        } else if !planes.text && !planes.callgraph && self.data_meaning_config().is_none() {
            Some(Withheld::IndexesOff)
        } else {
            None
        }
    }

    /// Holds the slot a bind holds while it scans, so a test sees the
    /// checkout as binding.
    #[cfg(test)]
    pub(crate) fn hold_data_checkout_slot_for_test(
        &self,
    ) -> parking_lot::MutexGuard<'_, Option<Arc<crate::data_layer::runtime::DataCheckout>>> {
        self.data_checkout.lock()
    }

    /// Passes the watcher's changed paths to the data layer, if this root
    /// has a bound checkout there.
    pub(crate) fn data_note_paths(&self, paths: &[PathBuf]) {
        let checkout = self.data_checkout.lock().clone();
        if let Some(checkout) = checkout {
            checkout.note_paths(paths);
        }
        self.data_children.note_paths(paths);
    }

    /// Tells the data layer the watcher lost events: each bound checkout,
    /// this root's and its child repositories', compares its files with its
    /// state again.
    pub(crate) fn data_rescan(&self) {
        let checkout = self.data_checkout.lock().clone();
        if let Some(checkout) = checkout {
            checkout.rescan();
        }
        self.data_children.rescan();
    }

    /// The data-layer checkouts of this parent folder's child repositories
    /// (see `data_layer::parent`).
    pub(crate) fn data_children(&self) -> &crate::data_layer::parent::ChildCheckouts {
        &self.data_children
    }

    /// The executor this root's background work runs on as its maintenance,
    /// once the daemon installed one; `None` in a standalone AFT.
    pub(crate) fn data_executor(&self) -> Option<crate::executor::RootMaintenanceExecutor> {
        self.callgraph_executor.lock().clone()
    }

    pub fn set_cache_role(&self, is_worktree_bridge: bool, git_common_dir: Option<PathBuf>) {
        *self.is_worktree_bridge.lock() = is_worktree_bridge;
        *self.linked_main_root.lock() = match (&git_common_dir, &self.config().project_root) {
            (Some(common), Some(root)) => verified_linked_main_root(root, common),
            _ => None,
        };
        *self.git_common_dir.lock() = git_common_dir;
        // The configure-time worktree probe already applies the test seam, so
        // the automatic TODO refresh follows the same effective root role as
        // callgraph cold-build gating while explicit inspect demand stays enabled.
        self.inspect_manager
            .set_automatic_tier2_refresh_allowed(!is_worktree_bridge);
        let artifact_read_only = self.shared_artifacts_read_only.load(Ordering::SeqCst);
        self.callgraph_writer
            .store(!is_worktree_bridge && !artifact_read_only, Ordering::SeqCst);
    }

    pub fn set_cache_writer_capabilities(&self, callgraph_writer: bool, inspect_writer: bool) {
        self.callgraph_writer
            .store(callgraph_writer, Ordering::SeqCst);
        self.inspect_writer.store(inspect_writer, Ordering::SeqCst);
    }

    pub fn callgraph_writer(&self) -> bool {
        self.callgraph_writer.load(Ordering::SeqCst)
    }

    pub fn inspect_writer(&self) -> bool {
        self.inspect_writer.load(Ordering::SeqCst)
    }

    /// Shared legacy artifacts remain read-only without their writer lease.
    /// A checkout may still contribute immutable blobs to its own scoped view.
    pub fn shared_artifacts_read_only(&self) -> bool {
        !self.callgraph_writer()
    }

    /// Mark whether this context serves standalone NDJSON requests rather than
    /// a live subc route. Only standalone queries may disclose a stale CLI
    /// snapshot instead of following the daemon's normal freshness path.
    #[doc(hidden)]
    pub fn set_daemonless_query_mode(&self, enabled: bool) {
        self.daemonless_query_mode.store(enabled, Ordering::SeqCst);
        self.reconcile_rerank_backend(&self.config_unpinned());
    }

    /// The search reranker's backend slot.
    pub(crate) fn rerank_slot(
        &self,
    ) -> &crate::search_state::rerank::slot::BackendSlot {
        &self.rerank_slot
    }

    /// Start building the reranker backend for `config` in the background when
    /// it differs from the one built or being built. Never blocks on the build.
    fn reconcile_rerank_backend(&self, config: &Config) {
        self.rerank_slot.reconcile(
            crate::search_state::rerank::slot::BuildInputs::from_config(
                config,
                !self.daemonless_query_mode(),
            ),
        );
    }

    pub(crate) fn daemonless_query_mode(&self) -> bool {
        self.daemonless_query_mode.load(Ordering::SeqCst)
    }

    /// True when this root is borrow-only and `worktree.ram_overlay` is on.
    ///
    /// Search and symbol-cache watcher arms may then apply local edits to the
    /// in-RAM trigram delta. Persist stays fail-closed: a borrow-only root
    /// never writes the shared `cache.bin`, overlay or not.
    pub fn ram_overlay_active(&self) -> bool {
        self.shared_artifacts_read_only() && self.config().worktree.ram_overlay
    }

    pub fn is_worktree_bridge(&self) -> bool {
        *self.is_worktree_bridge.lock()
    }

    pub fn git_common_dir(&self) -> Option<PathBuf> {
        self.git_common_dir.lock().clone()
    }

    /// Replace the current degraded-mode reasons. Empty vec = full-featured
    /// mode (no degradation). Called by `handle_configure` after deciding
    /// which subsystems to disable for this project root.
    pub fn set_degraded_reasons(&self, reasons: Vec<String>) {
        *self.degraded_reasons.lock() = reasons;
    }

    pub fn set_heavy_root_work_allowed(&self, allowed: bool) {
        self.heavy_root_work_allowed
            .store(allowed, Ordering::SeqCst);
    }

    pub fn heavy_root_work_allowed(&self) -> bool {
        self.heavy_root_work_allowed.load(Ordering::SeqCst)
            && !self.subc_lifecycle.is_unbound()
            && self
                .canonical_cache_root_opt()
                .or_else(|| self.config().project_root.clone())
                .is_none_or(|root| root.is_dir())
    }

    fn try_heavy_root_work_allowed(&self) -> Option<bool> {
        if !self.heavy_root_work_allowed.load(Ordering::SeqCst) {
            return Some(false);
        }
        self.subc_lifecycle.try_is_bound()
    }

    pub fn add_degraded_reason(&self, reason: impl Into<String>) -> bool {
        let reason = reason.into();
        let mut reasons = self.degraded_reasons.lock();
        if reasons.iter().any(|existing| existing == &reason) {
            return false;
        }
        reasons.push(reason);
        true
    }

    /// Swap one degraded reason for another in place, keeping the order.
    /// Returns whether `from` was present.
    pub(crate) fn replace_degraded_reason(&self, from: &str, to: &str) -> bool {
        let mut reasons = self.degraded_reasons.lock();
        let Some(index) = reasons.iter().position(|reason| reason == from) else {
            return false;
        };
        reasons[index] = to.to_string();
        true
    }

    /// Record a checkout view load that failed for a query on this project.
    /// The agent-visible error is returned unchanged by the caller; this only
    /// counts the failure (by plane and cause), keeps the last error and
    /// time, marks the project degraded until the next successful load, and
    /// logs at error level at most once a minute per plane.
    /// A measured disk-reserve refusal is recorded separately, without adding
    /// to the bug count or replacing the most recent genuine failure.
    pub(crate) fn note_view_load_failure(&self, plane: &'static str, error: &str) {
        self.note_classified_view_load_failure(plane, error, false);
    }

    fn note_classified_view_load_failure(&self, plane: &'static str, error: &str, planned: bool) {
        if let Some(condition) = crate::view_storage_reserve::DiskReserveCondition::observed(
            &self.storage_paths().artifacts,
            error,
        ) {
            self.view_load_health
                .lock()
                .disk_reserve
                .insert(plane.to_string(), condition);
            return;
        }
        if planned {
            return;
        }
        let cause = view_load_cause(error);
        let now = Instant::now();
        let (count, suppressed) = {
            let mut health = self.view_load_health.lock();
            let count = {
                let slot = health
                    .by_cause
                    .entry((plane.to_string(), cause.to_string()))
                    .or_default();
                *slot = slot.saturating_add(1);
                *slot
            };
            health.total = health.total.saturating_add(1);
            let last_at_ms = semantic_health_unix_millis();
            health.last = Some(ViewLoadFailure {
                plane: plane.to_string(),
                cause: cause.to_string(),
                error: error.to_string(),
                count,
                total: health.total,
                last_at_ms,
            });
            let entry = health.log_state.entry(plane.to_string()).or_default();
            if entry
                .0
                .is_some_and(|last| now.duration_since(last) < VIEW_LOAD_LOG_INTERVAL)
            {
                entry.1 = entry.1.saturating_add(1);
                (count, None)
            } else {
                let suppressed = std::mem::take(&mut entry.1);
                entry.0 = Some(now);
                (count, Some(suppressed))
            }
        };
        self.add_degraded_reason(VIEW_LOAD_DEGRADED_REASON);
        if let Some(suppressed) = suppressed {
            let root = self
                .canonical_cache_root_opt()
                .unwrap_or_default()
                .display()
                .to_string();
            crate::slog_error!(
                "view load failed root={} plane={} cause={} failures={} suppressed={} error={}",
                root,
                plane,
                cause,
                count,
                suppressed,
                error
            );
        }
    }

    /// Suppressed error-log repeats for `plane` since its last emission.
    /// Tests use it to prove the per-minute throttle without wall-clock waits.
    #[cfg(test)]
    pub(crate) fn view_load_suppressed_for_test(&self, plane: &str) -> u64 {
        self.view_load_health
            .lock()
            .log_state
            .get(plane)
            .map(|(_, suppressed)| *suppressed)
            .unwrap_or(0)
    }

    /// Snapshot of current degraded-mode reasons. Order is stable
    /// (insertion order from `set_degraded_reasons`) so UI rendering and
    /// snapshot diffs are deterministic.
    pub fn degraded_reasons(&self) -> Vec<String> {
        self.degraded_reasons.lock().clone()
    }

    /// True iff at least one degraded reason is recorded.
    pub fn is_degraded(&self) -> bool {
        !self.degraded_reasons.lock().is_empty()
    }

    /// True when configure identified the current root as exactly `$HOME`.
    /// Home is a user container, never a project root, so callgraph queries
    /// must report the intentional disabled state instead of a retryable miss.
    pub fn is_home_root(&self) -> bool {
        self.degraded_reasons
            .lock()
            .iter()
            .any(|reason| reason == "home_root")
    }

    pub fn root_too_broad(&self) -> bool {
        self.degraded_reasons
            .lock()
            .iter()
            .any(|reason| reason == "root_too_broad")
    }

    /// The bound root's artifact identity was provisional at its last bind
    /// (aft.db was busy or Git was unreadable), so its index lanes are off.
    /// A busy aft.db is retried in the background
    /// ([`crate::provisional_identity_retry`]); an unreadable Git identity
    /// waits for the next bind.
    pub(crate) fn artifact_identity_provisional(&self) -> bool {
        self.artifact_identity_provisional_reason().is_some()
    }

    pub(crate) fn artifact_identity_provisional_reason(&self) -> Option<String> {
        self.degraded_reasons
            .lock()
            .iter()
            .find(|reason| {
                reason.as_str() == crate::root_identity::key::PROVISIONAL_IDENTITY_REASON
                    || reason.as_str()
                        == crate::root_identity::key::PROVISIONAL_IDENTITY_EXHAUSTED_REASON
                    || reason
                        .starts_with(crate::root_identity::key::GIT_IDENTITY_UNAVAILABLE_REASON)
            })
            .cloned()
    }

    pub fn cache_role(&self) -> &'static str {
        if self.canonical_cache_root.lock().is_none() {
            "not_initialized"
        } else if self.is_worktree_bridge() {
            "worktree"
        } else if self.shared_artifacts_read_only.load(Ordering::SeqCst) {
            "read_only"
        } else {
            "main"
        }
    }

    /// Resolve the project root used for the callgraph store: prefer the
    /// canonical cache root, falling back to the configured project root.
    pub fn callgraph_project_root(&self) -> Option<PathBuf> {
        self.canonical_cache_root_opt().or_else(|| {
            self.config()
                .project_root
                .clone()
                .map(|root| std::fs::canonicalize(&root).unwrap_or(root))
        })
    }

    /// True only after explicit per-checkout activation, never from defaults.
    pub fn checkout_query_runtime_active(&self) -> bool {
        false
    }

    pub(crate) fn record_checkout_pending_watcher_gap(&self) {
        if self
            .watcher_counters()
            .checkout_watcher_gap_pending
            .swap(false, Ordering::AcqRel)
        {}
    }

    pub(crate) fn install_callgraph_executor(
        &self,
        executor: crate::executor::RootMaintenanceExecutor,
    ) {
        *self.callgraph_executor.lock() = Some(executor.clone());
        if let Some(checkout) = self.data_checkout.lock().as_ref() {
            checkout.set_executor(executor.clone());
        }
        self.data_children.set_executor(&executor);
    }

    #[cfg(test)]
    pub(crate) fn callgraph_executor_for_test(
        &self,
    ) -> Option<crate::executor::RootMaintenanceExecutor> {
        self.callgraph_executor.lock().clone()
    }

    /// Fence and drain standalone call-graph acquisition before process teardown.
    /// Daemon acquisitions are also cancelled, and their executor owns the drain.
    /// Gate every watcher-maintained artifact after the last route detaches. Files
    /// may change before the watcher is restored, so a later bind must reconcile
    /// from disk instead of serving retained snapshots that missed those edits.
    pub(crate) fn invalidate_artifacts_after_watcher_gap(&self) {
        self.next_semantic_persist_epoch();

        self.semantic_index
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        // Keep semantic status reloadable when the feature is enabled: the
        // query path's self-healing reload only fires from Ready (or Failed on
        // read-only roots), so Disabled would strand an already-bound root
        // with no way back short of a reconfigure. The advanced persist epoch
        // and strict verify memo force the reload to re-verify from disk.
        *self
            .semantic_index_status
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = if self.config().indexes.semantic
        {
            SemanticIndexStatus::ready()
        } else {
            SemanticIndexStatus::Disabled
        };

        if let Some(root) = self
            .canonical_cache_root_opt()
            .or_else(|| self.config().project_root.clone())
        {
            crate::cache_freshness::invalidate_verify_memo_strict(&root);
        }
        self.inspect_manager.evict_idle_caches();
        self.reset_symbol_cache();
        self.clear_tsconfig_membership_cache();
    }

    /// The Git checkout holding `requested_path`, resolved from the session's
    /// project root. A plain root the policy protects is refused.
    pub(crate) fn resolve_external_git_root(
        &self,
        project_root: &Path,
        requested_path: &str,
        scan_root_is_protected: impl Fn(&Path) -> bool,
    ) -> Result<PathBuf, crate::git_root_resolution::GitRootResolutionError> {
        crate::git_root_resolution::resolve_git_root_from_user_path(
            project_root,
            requested_path,
            scan_root_is_protected,
        )
    }

    pub(crate) fn search_exact_memo(
        &self,
    ) -> Arc<crate::search_state::memo::ExactMemoStore> {
        Arc::clone(&self.search_exact_memo)
    }

    pub fn add_pending_semantic_index_paths<I>(&self, paths: I)
    where
        I: IntoIterator<Item = PathBuf>,
    {
        let paths = paths.into_iter().collect::<Vec<_>>();
        if !paths.is_empty() {
            self.invalidate_warm_verify_memo();
            self.pending_semantic_index_paths.lock().extend(paths);
        }
    }

    pub(crate) fn invalidate_warm_verify_memo(&self) {
        if let Some(root) = self.canonical_cache_root_opt() {
            crate::cache_freshness::invalidate_verify_memo(&root);
        }
    }

    pub fn take_pending_semantic_index_paths(&self) -> Vec<PathBuf> {
        std::mem::take(&mut *self.pending_semantic_index_paths.lock())
            .into_iter()
            .collect()
    }

    pub fn mark_pending_semantic_corpus_refresh(&self) {
        *self.pending_semantic_corpus_refresh.lock() = true;
    }

    pub fn take_pending_semantic_corpus_refresh(&self) -> bool {
        std::mem::take(&mut *self.pending_semantic_corpus_refresh.lock())
    }

    /// Copies of the pending Tier-2 and semantic path sets and the
    /// corpus-refresh flag, unfiltered and left in place.
    #[cfg(test)]
    pub(crate) fn pending_reconciliation_snapshot_for_test(
        &self,
    ) -> (BTreeSet<PathBuf>, BTreeSet<PathBuf>, bool) {
        (
            self.pending_tier2_paths.lock().clone(),
            self.pending_semantic_index_paths.lock().clone(),
            *self.pending_semantic_corpus_refresh.lock(),
        )
    }

    pub fn clear_pending_index_updates(&self) {
        self.pending_tier2_paths.lock().clear();
        self.pending_semantic_index_paths.lock().clear();
        *self.pending_semantic_corpus_refresh.lock() = false;
    }

    pub(crate) fn restore_pending_reconciliation_state(&self, state: PendingReconciliationState) {
        restore_pending_paths(&mut self.pending_tier2_paths.lock(), state.tier2);
        restore_pending_paths(
            &mut self.pending_semantic_index_paths.lock(),
            state.semantic,
        );
        if state.corpus_refresh {
            *self.pending_semantic_corpus_refresh.lock() = true;
        }
    }

    /// Cancel artifact work that no longer has a bound daemon route to consume it.
    /// `mark_subc_unbound` advances the generation under the lifecycle admission
    /// gate before this cleanup runs. Clearing receivers lets a later rebind
    /// schedule fresh work instead of adopting a disconnected worker forever.
    ///
    /// Pending watcher-derived path sets are RETAINED: a pre-unbind artifact
    /// worker may legitimately finish generation-safe disk persistence during
    /// the unbound window (content generation and persist epochs deliberately
    /// do not advance on route teardown), and those paths are the only record
    /// that its artifact is content-stale. Rebind replays them. Disposal of
    /// pending state belongs to non-equivalent configure and TTL eviction
    /// (transactional take in the TTL reaper), whose strict invalidation
    /// subsumes their purpose.
    pub(crate) fn cancel_unbound_artifact_work(&self) {
        if let Some(root) = self.canonical_cache_root_opt() {
            crate::cold_build_limiter::progress::cancel_root(&root);
        }
        let semantic_cancelled = self.semantic_index_rx.lock().is_some();
        self.retire_semantic_index_rx("the root was unbound");
        let semantic_refresh_cancelled = self.semantic_refresh_event_rx.lock().is_some();
        self.clear_semantic_refresh_worker();
        *self.semantic_embedding_model.lock() = None;
        self.reset_semantic_cold_seed_gate_for_configure();
        let _ = self.inspect_manager.discard_completions();
        if semantic_cancelled || semantic_refresh_cancelled {
            let has_index = self
                .semantic_index
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_some();
            // In-flight refreshing files were consumed from the watcher; the
            // cancelled worker will never re-embed them. Transfer them to the
            // retained pending set so the rebind's replacement worker does.
            {
                let mut status = self
                    .semantic_index_status
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let refreshing = status.take_refreshing_files();
                if !refreshing.is_empty() {
                    self.pending_semantic_index_paths.lock().extend(refreshing);
                }
                if status.corpus_refresh_in_flight() {
                    *self.pending_semantic_corpus_refresh.lock() = true;
                }
                *status = if has_index {
                    SemanticIndexStatus::ready()
                } else {
                    SemanticIndexStatus::Disabled
                };
            }
            self.set_semantic_build_progress(None);
        }
    }

    pub fn inspect_manager(&self) -> Arc<InspectManager> {
        Arc::clone(&self.inspect_manager)
    }

    /// Standing ownership exempts a root from idle artifact eviction only. It
    /// does not bypass strict verification, budgets, breaker checks, or leases.
    pub(crate) fn set_standing_artifact_exempt(&self, exempt: bool) {
        self.standing_artifact_exempt
            .store(exempt, Ordering::Release);
    }

    pub(crate) fn cold_build_limiter(&self) -> Arc<crate::cold_build_limiter::ColdBuildLimiter> {
        Arc::clone(
            &self
                .cold_build_limiter
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }

    /// Give one integration-test context its own maintenance-build capacity.
    /// Production contexts continue to share the process-wide limiter.
    #[doc(hidden)]
    pub fn isolate_cold_build_limiter_for_test(&self, limit: usize) {
        let limiter = crate::cold_build_limiter::isolated_limiter(limit);
        self.inspect_manager
            .set_cold_build_limiter(Arc::clone(&limiter));
        *self
            .cold_build_limiter
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = limiter;
    }

    /// Takes one slot of this context's maintenance-build limiter, so an
    /// integration test can hold queued background work (such as a legacy
    /// import) at its admission point and observe what queries report
    /// meanwhile. `None` when every slot is taken.
    #[doc(hidden)]
    pub fn take_cold_build_slot_for_test(
        &self,
    ) -> Option<crate::cold_build_limiter::ColdBuildPermit> {
        self.cold_build_limiter().try_acquire()
    }

    pub fn add_pending_tier2_paths<I>(&self, paths: I)
    where
        I: IntoIterator<Item = PathBuf>,
    {
        self.pending_tier2_paths.lock().extend(paths);
    }

    pub fn pending_tier2_paths(&self) -> Vec<PathBuf> {
        self.pending_tier2_paths.lock().iter().cloned().collect()
    }

    pub fn remove_pending_tier2_paths<I>(&self, paths: I)
    where
        I: IntoIterator<Item = PathBuf>,
    {
        let mut pending = self.pending_tier2_paths.lock();
        for path in paths {
            pending.remove(&path);
        }
    }

    pub fn reset_tier2_refresh_scheduler(&self) {
        let now = Instant::now();
        let cold_ready_at = crate::inspect::tier2_scheduler::process_tier2_cold_start_pacer()
            .lock()
            .reserve(now);
        self.tier2_refresh_scheduler
            .lock()
            .reset_after_configure_with_cold_ready_at(cold_ready_at);
    }

    /// Reset as of `now` with the plain cold-cache delay, outside the
    /// process-wide pacing. Tests use this to place deadlines exactly.
    #[doc(hidden)]
    pub fn reset_tier2_refresh_scheduler_at(&self, now: Instant) {
        self.tier2_refresh_scheduler
            .lock()
            .reset_after_configure(now);
    }

    pub fn request_tier2_refresh_pull(&self) -> bool {
        let can_schedule = self.inspect_writer()
            && self.heavy_root_work_allowed()
            && self.inspect_manager.automatic_tier2_refresh_allowed();
        self.tier2_refresh_scheduler
            .lock()
            .request_pull(can_schedule)
    }

    pub fn tick_tier2_refresh_scheduler(
        &self,
        changed_path_count: usize,
    ) -> Option<Tier2TriggerReason> {
        self.tick_tier2_refresh_scheduler_at(Instant::now(), changed_path_count)
    }

    #[doc(hidden)]
    pub fn tick_tier2_refresh_scheduler_at(
        &self,
        now: Instant,
        changed_path_count: usize,
    ) -> Option<Tier2TriggerReason> {
        let manager = self.inspect_manager();
        let can_write = self.inspect_writer()
            && self.heavy_root_work_allowed()
            && manager.automatic_tier2_refresh_allowed();
        let in_flight = manager.tier2_any_in_flight();
        let gates = Tier2ExternalGates {
            semantic_cold_seed_active: self.semantic_cold_seed_active(),
        };
        let mut scheduler = self.tier2_refresh_scheduler.lock();
        let decision =
            scheduler.tick_with_gates(now, changed_path_count, can_write, in_flight, gates);
        // A deadline the scheduler works out and then does not honour is
        // invisible otherwise. Name the predicate that held it, once per
        // overdue stretch.
        let overdue_block = if decision.is_none() {
            scheduler.take_overdue_dispatch_block(now, can_write, in_flight, gates)
        } else {
            None
        };
        drop(scheduler);

        if let Some(block) = overdue_block {
            crate::slog_info!(
                "todos refresh deadline passed without dispatch: blocked_by={}, pending_paths={}",
                block.as_str(),
                self.pending_tier2_paths
                    .try_lock()
                    .map(|paths| paths.len().to_string())
                    .unwrap_or_else(|| "busy".to_string())
            );
        }

        if let Some(reason) = decision {
            self.start_tier2_refresh(reason, manager);
        }

        decision
    }

    /// True when the refresh this root's scheduler has already promised is
    /// due and no external gate is holding it back.
    ///
    /// The maintenance tick probes this so a due refresh gets a watcher drain
    /// pass to run in. The scheduler is ticked only from that drain, and the
    /// drain is otherwise enqueued only when the watcher has paths to apply —
    /// so on a root that falls quiet after its last edit, the debounce deadline
    /// passes with nothing left to observe it and the refresh waits for the
    /// next file change. Mirroring the dispatch gates here keeps a blocked root
    /// (read-only, analysis in flight, cold seed) from asking for a drain pass
    /// several times a second.
    pub fn tier2_refresh_dispatch_due(&self) -> bool {
        if !self.inspect_writer()
            || self.try_heavy_root_work_allowed() != Some(true)
            || !self.inspect_manager.automatic_tier2_refresh_enabled()
            || self.semantic_cold_seed_active()
        {
            return false;
        }
        // A contended source is busy with the very work a dispatch would wait
        // for; the next maintenance tick re-probes.
        if self.inspect_manager.try_tier2_any_in_flight() != Some(false) {
            return false;
        }
        // The scheduler itself is only held for short bookkeeping, so its
        // contention says nothing about whether a refresh is due. Fail open
        // like the other maintenance probes: the watcher drain re-reads the
        // scheduler under its own lock and does nothing if no refresh is due.
        self.tier2_refresh_scheduler
            .try_lock()
            .is_none_or(|scheduler| scheduler.dispatch_due(Instant::now()))
    }

    pub fn note_tier2_refresh_started(&self) {
        self.note_tier2_refresh_started_at(Instant::now());
    }

    #[doc(hidden)]
    pub fn note_tier2_refresh_started_at(&self, now: Instant) {
        self.tier2_refresh_scheduler
            .lock()
            .note_external_scan_started(now);
    }

    pub fn tier2_trigger_reason(&self) -> Option<&'static str> {
        self.tier2_refresh_scheduler
            .lock()
            .last_trigger_reason()
            .map(Tier2TriggerReason::as_str)
    }

    #[doc(hidden)]
    pub fn tier2_pull_demand_pending(&self) -> bool {
        self.tier2_refresh_scheduler.lock().pull_demand_pending()
    }

    fn start_tier2_refresh(&self, reason: Tier2TriggerReason, manager: Arc<InspectManager>) {
        let generation = self.configure_generation();
        if !self.inspect_writer()
            || !self.heavy_root_work_allowed()
            || !manager.automatic_tier2_refresh_allowed()
            || !self.config().inspect.enabled
        {
            return;
        }
        let _ = self.run_if_subc_bound_generation(generation, || {
            self.start_tier2_refresh_admitted(reason, manager);
        });
    }

    /// Refresh the status bar's TODO count, which only an explicit
    /// `aft_inspect` would otherwise compute. This is the only analysis the
    /// scheduler runs: dead code, unused exports, duplicates, cycles and
    /// complexity cost seconds and hundreds of MB on a large repository, so
    /// they run only when an agent asks for them. The completion is drained
    /// with the other inspect completions, which refreshes the bar.
    fn start_tier2_refresh_admitted(
        &self,
        reason: Tier2TriggerReason,
        manager: Arc<InspectManager>,
    ) {
        let Some(snapshot) = self.tier2_refresh_snapshot() else {
            return;
        };
        if !snapshot
            .config
            .inspect
            .category_enabled(InspectCategory::Todos)
        {
            return;
        }
        match manager.submit_background(
            snapshot.clone(),
            InspectCategory::Todos,
            crate::inspect::JobScope::for_project(snapshot.project_root.clone()),
        ) {
            Ok(_) => crate::slog_info!("todos refresh scheduled: reason={}", reason.as_str()),
            Err(error) => crate::slog_info!("todos refresh not scheduled: {}", error),
        }
    }

    fn tier2_refresh_snapshot(&self) -> Option<InspectSnapshot> {
        self.harness_opt()?;
        let config = self.config();
        let project_root = config
            .project_root
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        // Normalized, not bare-canonical: scoped diagnostics compare
        // LSP-reported paths (normalized form) against this root with
        // starts_with, and a verbatim root on Windows matches nothing.
        let project_root = crate::path_identity::canonicalize_normalized(&project_root);
        Some(InspectSnapshot::new_with_capabilities(
            project_root,
            self.inspect_dir(),
            config,
            self.symbol_cache(),
            self.inspect_writer(),
            self.callgraph_writer(),
        ))
    }

    /// Access the shared symbol cache.
    pub fn symbol_cache(&self) -> SharedSymbolCache {
        Arc::clone(&self.symbol_cache)
    }

    /// Clear the shared symbol cache and return the new active generation.
    pub fn reset_symbol_cache(&self) -> u64 {
        self.symbol_cache
            .write()
            .map(|mut cache| cache.reset())
            .unwrap_or(0)
    }

    /// Access the semantic search index.
    pub fn semantic_index(&self) -> &RwLock<Option<SemanticIndex>> {
        &self.semantic_index
    }

    /// Access the semantic-index build receiver.
    pub fn semantic_index_rx(
        &self,
    ) -> &parking_lot::Mutex<Option<crossbeam_channel::Receiver<SemanticIndexEvent>>> {
        &self.semantic_index_rx
    }

    /// Drop the semantic-index receiver. A finished load still waiting in it
    /// is freed with it and logged with `reason`.
    pub(crate) fn retire_semantic_index_rx(&self, reason: &str) {
        let mut receiver = self.semantic_index_rx.lock();
        let discarded = take_undelivered_install(
            &self.semantic_pending_install,
            self.semantic_index_rx_epoch(),
            receiver.as_ref(),
        );
        *receiver = None;
        self.next_semantic_index_rx_epoch();
        drop(receiver);
        log_discarded_pending_install(
            "semantic index",
            self.canonical_cache_root_opt().as_deref(),
            discarded,
            reason,
        );
    }

    /// Rebind a live semantic build to a newer configure generation when the
    /// semantic corpus inputs are unchanged. Its receiver keeps the completed
    /// result while the worker's dedicated build epoch remains valid.
    pub(crate) fn adopt_semantic_index_rx_generation(&self, generation: u64) -> bool {
        let receiver = self.semantic_index_rx.lock();
        if receiver.is_none() {
            return false;
        }
        self.note_semantic_index_rx_generation(generation);
        true
    }

    pub(crate) fn note_semantic_index_rx_generation(&self, generation: u64) {
        self.semantic_index_rx_generation
            .store(generation, Ordering::SeqCst);
    }

    pub(crate) fn semantic_index_rx_generation(&self) -> u64 {
        self.semantic_index_rx_generation.load(Ordering::SeqCst)
    }

    pub(crate) fn next_semantic_index_rx_epoch(&self) -> u64 {
        self.semantic_index_rx_epoch
            .fetch_add(1, Ordering::SeqCst)
            .wrapping_add(1)
    }

    pub(crate) fn semantic_index_rx_epoch(&self) -> u64 {
        self.semantic_index_rx_epoch.load(Ordering::SeqCst)
    }

    pub(crate) fn next_semantic_persist_epoch(&self) -> u64 {
        self.semantic_persist_epoch.next()
    }

    pub fn semantic_index_status(&self) -> &RwLock<SemanticIndexStatus> {
        self.semantic_index_status.as_ref()
    }

    pub(crate) fn set_semantic_build_progress(&self, progress: Option<SemanticBuildProgress>) {
        *self
            .semantic_build_progress
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = progress;
    }

    pub(crate) fn semantic_build_progress(&self) -> Option<SemanticBuildProgress> {
        self.semantic_build_progress
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Reset this context's cold semantic seed gate for a newly accepted
    /// configure and return the generation token for the worker being spawned.
    pub fn reset_semantic_cold_seed_gate_for_configure(&self) -> u64 {
        self.semantic_cold_seed_active
            .store(false, Ordering::SeqCst);
        self.semantic_cold_seed_generation
            .fetch_add(1, Ordering::SeqCst)
            .wrapping_add(1)
    }

    pub fn semantic_cold_seed_active_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.semantic_cold_seed_active)
    }

    pub fn semantic_cold_seed_generation_flag(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.semantic_cold_seed_generation)
    }

    pub fn semantic_cold_seed_generation(&self) -> u64 {
        self.semantic_cold_seed_generation.load(Ordering::SeqCst)
    }

    pub fn semantic_cold_seed_active(&self) -> bool {
        self.semantic_cold_seed_active.load(Ordering::SeqCst)
    }

    pub fn schedule_semantic_cold_seed_gate_for_configure(&self) {
        self.semantic_cold_seed_active.store(true, Ordering::SeqCst);
    }

    /// Clear the cold-seed gate and resume work that was intentionally held back
    /// while the full semantic corpus was accumulating. This entry point is used
    /// by the code that drains events from the semantic worker.
    pub fn clear_semantic_cold_seed_gate_and_resume_deferred_work(&self) {
        self.resume_semantic_cold_seed_deferred_work(false);
    }

    /// Resume work after the semantic worker has already cleared the atomic gate
    /// itself, such as on cached-index load or before a retry backoff sleep.
    pub fn resume_deferred_work_after_semantic_cold_seed_gate_cleared(&self) {
        self.resume_semantic_cold_seed_deferred_work(true);
    }

    pub(crate) fn take_semantic_cold_seed_resume(&self, force: bool) -> SemanticColdSeedResume {
        let was_active = self.semantic_cold_seed_active.swap(false, Ordering::SeqCst);
        SemanticColdSeedResume {
            request_tier2: force || was_active,
        }
    }

    pub(crate) fn apply_semantic_cold_seed_resume(&self, resume: SemanticColdSeedResume) {
        if resume.request_tier2 {
            let _ = self.request_tier2_refresh_pull();
        }
    }

    fn resume_semantic_cold_seed_deferred_work(&self, force: bool) {
        let resume = self.take_semantic_cold_seed_resume(force);
        self.apply_semantic_cold_seed_resume(resume);
    }

    #[doc(hidden)]
    pub fn set_semantic_cold_seed_active_for_test(&self, active: bool) {
        self.semantic_cold_seed_active
            .store(active, Ordering::SeqCst);
    }

    pub fn install_semantic_refresh_worker(
        &self,
        sender: crossbeam_channel::Sender<SemanticRefreshRequest>,
        event_rx: crossbeam_channel::Receiver<SemanticRefreshEvent>,
        worker_slot: SemanticRefreshWorkerSlot,
    ) {
        self.install_semantic_refresh_worker_for_build_epoch(
            sender,
            event_rx,
            worker_slot,
            self.semantic_index_rx_epoch(),
        );
    }

    pub(crate) fn install_semantic_refresh_worker_for_build_epoch(
        &self,
        sender: crossbeam_channel::Sender<SemanticRefreshRequest>,
        event_rx: crossbeam_channel::Receiver<SemanticRefreshEvent>,
        worker_slot: SemanticRefreshWorkerSlot,
        build_epoch: u64,
    ) {
        self.clear_semantic_refresh_worker();
        {
            let mut receiver = self.semantic_refresh_event_rx.lock();
            let mut request = self.semantic_refresh_tx.lock();
            let mut worker = self.semantic_refresh_worker.lock();
            self.semantic_refresh_generation
                .store(self.configure_generation(), Ordering::SeqCst);
            self.semantic_refresh_epoch.fetch_add(1, Ordering::SeqCst);
            self.semantic_refresh_build_epoch
                .store(build_epoch, Ordering::SeqCst);
            *receiver = Some(event_rx);
            *request = Some(sender);
            *worker = Some(worker_slot);
        }
    }

    pub(crate) fn semantic_refresh_generation(&self) -> u64 {
        self.semantic_refresh_generation.load(Ordering::SeqCst)
    }

    pub fn clear_semantic_refresh_worker(&self) {
        let worker_slot = {
            let mut receiver = self.semantic_refresh_event_rx.lock();
            let mut request = self.semantic_refresh_tx.lock();
            let mut worker = self.semantic_refresh_worker.lock();
            *receiver = None;
            *request = None;
            self.semantic_refresh_epoch.fetch_add(1, Ordering::SeqCst);
            self.semantic_refresh_build_epoch.store(0, Ordering::SeqCst);
            self.invalidate_semantic_refresh_probe();
            worker.take()
        };
        if let Some(worker_slot) = worker_slot {
            if let Ok(mut handle) = worker_slot.lock() {
                drop(handle.take());
            }
        }
    }

    pub fn semantic_refresh_sender(
        &self,
    ) -> Option<crossbeam_channel::Sender<SemanticRefreshRequest>> {
        self.semantic_refresh_tx.lock().clone()
    }

    pub(crate) fn semantic_refresh_retry_slots(
        &self,
    ) -> (
        Arc<parking_lot::Mutex<Option<crossbeam_channel::Sender<SemanticRefreshRequest>>>>,
        Arc<parking_lot::Mutex<BTreeSet<PathBuf>>>,
    ) {
        (
            Arc::clone(&self.semantic_refresh_tx),
            Arc::clone(&self.pending_semantic_index_paths),
        )
    }

    pub fn semantic_refresh_event_rx(
        &self,
    ) -> &parking_lot::Mutex<Option<crossbeam_channel::Receiver<SemanticRefreshEvent>>> {
        &self.semantic_refresh_event_rx
    }

    pub fn with_semantic_refresh_retry_attempts_mut<R>(
        &self,
        f: impl FnOnce(&mut BTreeMap<PathBuf, usize>) -> R,
    ) -> R {
        let mut attempts = self.semantic_refresh_retry_attempts.lock();
        f(&mut attempts)
    }

    pub fn clear_semantic_refresh_retry_attempts(&self, paths: &[PathBuf]) {
        let mut attempts = self.semantic_refresh_retry_attempts.lock();
        for path in paths {
            attempts.remove(path);
        }
    }

    pub fn clear_all_semantic_refresh_retry_attempts(&self) {
        self.semantic_refresh_retry_attempts.lock().clear();
    }

    pub fn semantic_refresh_circuit_is_open(&self) -> bool {
        self.semantic_refresh_circuit.open.load(Ordering::SeqCst)
    }

    fn record_embedding_backend_outage(&self, reason: &str) {
        let now_ms = semantic_health_unix_millis();
        let mut outage = self
            .semantic_refresh_circuit
            .backend_outage
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match outage.as_mut() {
            Some(outage) => {
                outage.last_error = reason.to_string();
                outage.next_retry_ms = outage.next_retry_ms.max(now_ms);
            }
            None => {
                *outage = Some(SemanticBackendOutage {
                    last_error: reason.to_string(),
                    since_ms: now_ms,
                    next_retry_ms: now_ms,
                });
            }
        }
    }

    fn set_embedding_backend_next_retry(&self, next_retry_ms: u64) {
        if let Ok(mut outage) = self.semantic_refresh_circuit.backend_outage.write() {
            if let Some(outage) = outage.as_mut() {
                outage.next_retry_ms = next_retry_ms;
            }
        }
    }

    pub(crate) fn semantic_backend_health_snapshot(&self) -> EmbeddingBackendHealthSnapshot {
        let outage = self
            .semantic_refresh_circuit
            .backend_outage
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let snapshot = match outage.as_ref() {
            Some(outage) => EmbeddingBackendHealthSnapshot {
                available: false,
                last_error: Some(outage.last_error.clone()),
                since_ms: Some(outage.since_ms),
                next_retry_ms: Some(outage.next_retry_ms),
                paused_until_ms: None,
                pause_cause: None,
            },
            None => EmbeddingBackendHealthSnapshot {
                available: true,
                ..EmbeddingBackendHealthSnapshot::default()
            },
        };
        drop(outage);
        let root = self.canonical_cache_root_opt();
        with_cold_build_backend_health(snapshot, root.as_deref())
    }

    pub(crate) fn try_semantic_backend_health_snapshot(
        &self,
    ) -> Option<EmbeddingBackendHealthSnapshot> {
        let outage = self
            .semantic_refresh_circuit
            .backend_outage
            .try_read()
            .ok()?;
        let snapshot = match outage.as_ref() {
            Some(outage) => EmbeddingBackendHealthSnapshot {
                available: false,
                last_error: Some(outage.last_error.clone()),
                since_ms: Some(outage.since_ms),
                next_retry_ms: Some(outage.next_retry_ms),
                paused_until_ms: None,
                pause_cause: None,
            },
            None => EmbeddingBackendHealthSnapshot {
                available: true,
                ..EmbeddingBackendHealthSnapshot::default()
            },
        };
        drop(outage);
        let root = self.canonical_cache_root.try_lock()?.clone();
        Some(with_cold_build_backend_health(snapshot, root.as_deref()))
    }

    pub fn record_semantic_refresh_transient_failure(
        &self,
        trip_threshold: usize,
        reason: &str,
    ) -> bool {
        let failures = self
            .semantic_refresh_circuit
            .consecutive_transient_failures
            .fetch_add(1, Ordering::SeqCst)
            .saturating_add(1);
        if failures >= trip_threshold {
            self.record_embedding_backend_outage(reason);
        }
        if failures >= trip_threshold
            && !self
                .semantic_refresh_circuit
                .open
                .swap(true, Ordering::SeqCst)
        {
            crate::slog_warn!(
                "embedding backend appears down: {}; suspending active retries, will resume on next change or successful probe",
                reason,
            );
        }
        self.semantic_refresh_circuit_is_open()
    }

    pub fn trip_semantic_refresh_circuit(&self, trip_threshold: usize, reason: &str) {
        self.semantic_refresh_circuit
            .consecutive_transient_failures
            .store(trip_threshold, Ordering::SeqCst);
        self.record_embedding_backend_outage(reason);
        if !self
            .semantic_refresh_circuit
            .open
            .swap(true, Ordering::SeqCst)
        {
            crate::slog_warn!(
                "embedding backend appears down: {}; suspending active retries, will resume on next change or successful probe",
                reason,
            );
        }
    }

    pub fn reset_semantic_refresh_transient_failure_count(&self) {
        self.semantic_refresh_circuit
            .consecutive_transient_failures
            .store(0, Ordering::SeqCst);
    }

    pub fn reset_semantic_refresh_circuit_after_success(&self) {
        self.reset_semantic_refresh_transient_failure_count();
        self.semantic_refresh_circuit
            .probe_ready
            .store(false, Ordering::SeqCst);
        *self
            .semantic_refresh_circuit
            .backend_outage
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        if self
            .semantic_refresh_circuit
            .open
            .swap(false, Ordering::SeqCst)
        {
            crate::slog_info!("embedding backend recovered; resuming normal refresh retries");
        }
    }

    pub fn semantic_refresh_transient_failure_count(&self) -> usize {
        self.semantic_refresh_circuit
            .consecutive_transient_failures
            .load(Ordering::SeqCst)
    }

    pub fn semantic_refresh_probe_is_scheduled(&self) -> bool {
        self.semantic_refresh_circuit
            .probe_in_flight
            .load(Ordering::SeqCst)
            || self.semantic_refresh_probe_ready()
    }

    pub fn semantic_refresh_probe_ready(&self) -> bool {
        self.semantic_refresh_circuit
            .probe_ready
            .load(Ordering::SeqCst)
    }

    pub fn take_semantic_refresh_probe_ready(&self) -> bool {
        self.semantic_refresh_circuit
            .probe_ready
            .swap(false, Ordering::SeqCst)
    }

    fn invalidate_semantic_refresh_probe(&self) {
        self.semantic_refresh_circuit
            .probe_token
            .fetch_add(1, Ordering::SeqCst);
        self.semantic_refresh_circuit
            .probe_ready
            .store(false, Ordering::SeqCst);
        self.semantic_refresh_circuit
            .probe_in_flight
            .store(false, Ordering::SeqCst);
    }

    pub fn ensure_semantic_refresh_probe_scheduled(&self, delay: Duration) {
        let receiver = self.semantic_refresh_event_rx.lock();
        if receiver.is_none()
            || self
                .semantic_refresh_circuit
                .probe_ready
                .load(Ordering::SeqCst)
            || self
                .semantic_refresh_circuit
                .probe_in_flight
                .swap(true, Ordering::SeqCst)
        {
            return;
        }
        let probe_token = self
            .semantic_refresh_circuit
            .probe_token
            .fetch_add(1, Ordering::SeqCst)
            .wrapping_add(1);
        self.set_embedding_backend_next_retry(
            semantic_health_unix_millis()
                .saturating_add(delay.as_millis().min(u128::from(u64::MAX)) as u64),
        );
        drop(receiver);

        let circuit = Arc::clone(&self.semantic_refresh_circuit);
        let session_id = crate::log_ctx::current_session();
        std::thread::spawn(move || {
            crate::log_ctx::with_session(session_id, || {
                std::thread::sleep(delay);
                if circuit.probe_token.load(Ordering::SeqCst) == probe_token {
                    circuit.probe_ready.store(true, Ordering::SeqCst);
                    circuit.probe_in_flight.store(false, Ordering::SeqCst);
                }
            });
        });
    }

    /// Access the cached semantic embedding model.
    pub fn semantic_embedding_model(
        &self,
    ) -> &parking_lot::Mutex<Option<crate::semantic_index::EmbeddingModel>> {
        &self.semantic_embedding_model
    }

    /// Access the file watcher handle (kept alive to continue watching).
    pub fn watcher(&self) -> &parking_lot::Mutex<Option<RecommendedWatcher>> {
        &self.watcher
    }

    pub(crate) fn watcher_counters(&self) -> Arc<WatcherCounters> {
        Arc::clone(
            &self
                .watcher_counters
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }

    /// Access the pre-filtered watcher event receiver.
    pub fn watcher_rx(
        &self,
    ) -> &parking_lot::Mutex<Option<crossbeam_channel::Receiver<WatcherDispatchEvent>>> {
        &self.watcher_rx
    }

    /// Access continuation state for the bounded watcher drain.
    pub(crate) fn watcher_drain_slice(
        &self,
    ) -> &parking_lot::Mutex<Option<WatcherDrainSliceState>> {
        &self.watcher_drain_slice
    }

    /// Include partially consumed dispatch events when reporting drain backlog.
    pub fn watcher_drain_pending_path_count(&self) -> usize {
        self.watcher_drain_slice.lock().as_ref().map_or(0, |state| {
            let active_paths = match &state.phase {
                WatcherDrainPhase::Collect => 0,
                WatcherDrainPhase::Apply { paths, .. } => paths.len(),
            };
            active_paths + state.pending_paths.len()
        })
    }

    /// Number of path-budgeted watcher batches since this runtime was installed.
    pub fn watcher_drain_path_slice_count(&self) -> usize {
        self.watcher_drain_slice
            .lock()
            .as_ref()
            .map_or(0, |state| state.path_slice_count)
    }

    /// Whether the watcher retained diagnostics work after failing to acquire
    /// the LSP manager. Merely reaching the phase at a slice boundary is not enough.
    #[doc(hidden)]
    pub fn watcher_lsp_diagnostics_retry_pending_for_test(&self) -> bool {
        self.watcher_drain_slice
            .lock()
            .as_ref()
            .is_some_and(|state| {
                state.lsp_diagnostics_lock_contended
                    && matches!(
                        &state.phase,
                        WatcherDrainPhase::Apply {
                            stage: WatcherDrainApplyPhase::LspDiagnostics,
                            remaining,
                            ..
                        } if *remaining > 0
                    )
            })
    }

    /// Install a watcher filter thread and its dispatch receiver. The caller
    /// must have stopped any previous watcher runtime first.
    pub fn install_watcher_runtime(
        &self,
        rx: crossbeam_channel::Receiver<WatcherDispatchEvent>,
        runtime: WatcherThreadHandle,
    ) {
        self.install_watcher_runtime_inner(rx, runtime, None);
    }

    pub(crate) fn install_watcher_runtime_with_thread_id(
        &self,
        rx: crossbeam_channel::Receiver<WatcherDispatchEvent>,
        runtime: WatcherThreadHandle,
        thread_id: std::thread::ThreadId,
    ) {
        self.install_watcher_runtime_inner(rx, runtime, Some(thread_id));
    }

    fn install_watcher_runtime_inner(
        &self,
        rx: crossbeam_channel::Receiver<WatcherDispatchEvent>,
        runtime: WatcherThreadHandle,
        _thread_id: Option<std::thread::ThreadId>,
    ) {
        let root = self.watcher_root_path();
        // Changes made while no watcher was installed were not observable.
        crate::lsp::manager::invalidate_applicability_walk(
            &root,
            crate::lsp::manager::InvalidationCause::WatcherInstalled,
        );
        let gitignore_generation = self.gitignore_generation.load(Ordering::SeqCst);
        let _runtime_guard = self.watcher_runtime_lock.lock();
        let replaced = self.watcher_thread.lock().replace(runtime);
        self.app.watcher_started();
        if let Some(runtime) = replaced {
            Self::spawn_watcher_shutdown(Arc::clone(&self.app), root.clone(), runtime);
        }
        *self.watcher_rx.lock() = Some(rx);
        *self.watcher_drain_slice.lock() = None;
        *self.watcher_runtime_identity.lock() = Some(WatcherRuntimeIdentity {
            root,
            gitignore_generation,
            #[cfg(test)]
            thread_id: _thread_id,
        });
    }

    pub(crate) fn watcher_runtime_matches(&self, root: &Path, gitignore_generation: u64) -> bool {
        let _runtime_guard = self.watcher_runtime_lock.lock();
        let thread_live = self
            .watcher_thread
            .lock()
            .as_ref()
            .is_some_and(|runtime| !runtime.is_finished());
        thread_live
            && self.watcher_rx.lock().is_some()
            && self
                .watcher_runtime_identity
                .lock()
                .as_ref()
                .is_some_and(|identity| {
                    identity.root == root && identity.gitignore_generation == gitignore_generation
                })
    }

    #[cfg(test)]
    pub(crate) fn watcher_runtime_thread_id_for_test(&self) -> Option<std::thread::ThreadId> {
        self.watcher_runtime_identity
            .lock()
            .as_ref()
            .and_then(|identity| identity.thread_id)
    }

    fn watcher_root_path(&self) -> PathBuf {
        self.canonical_cache_root_opt()
            .or_else(|| self.config().project_root.clone())
            .unwrap_or_else(|| PathBuf::from("<unconfigured>"))
    }

    // On Windows, detached teardown workers can appear under the same Rust
    // thread-start routine, so test-only names make them distinguishable.
    #[cfg(all(test, windows))]
    fn describe_teardown_thread_for_test(name: &str) {
        use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadDescription};
        let name: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        // SAFETY: the pseudo-handle names this thread and name is NUL-terminated.
        unsafe { SetThreadDescription(GetCurrentThread(), name.as_ptr()) };
    }

    fn spawn_watcher_shutdown(app: Arc<App>, root: PathBuf, runtime: WatcherThreadHandle) {
        const JOIN_TIMEOUT: Duration = Duration::from_secs(2);
        // Signal the watcher before scheduling the joiner so teardown does not
        // depend on a newly spawned thread winning CPU time under fleet load.
        runtime.request_shutdown();
        std::thread::spawn(move || {
            #[cfg(all(test, windows))]
            Self::describe_teardown_thread_for_test("aft-watcher-shutdown-join");
            match runtime.shutdown_and_join_timeout(JOIN_TIMEOUT) {
                WatcherJoinOutcome::Joined => {
                    app.watcher_stopped();
                    crate::slog_info!("watcher stopped: {}", root.display());
                }
                WatcherJoinOutcome::TimedOut(join) => {
                    crate::slog_warn!(
                        "watcher stop timed out after {} ms: {}",
                        JOIN_TIMEOUT.as_millis(),
                        root.display()
                    );
                    std::thread::spawn(move || {
                        #[cfg(all(test, windows))]
                        Self::describe_teardown_thread_for_test("aft-watcher-final-join");
                        let _ = join.join();
                        app.watcher_stopped();
                        crate::slog_info!("watcher stopped: {}", root.display());
                    });
                }
            }
        });
    }

    fn take_watcher_runtime(&self) -> Option<WatcherThreadHandle> {
        let _runtime_guard = self.watcher_runtime_lock.lock();
        let runtime = self.watcher_thread.lock().take();
        *self.watcher_rx.lock() = None;
        *self.watcher_drain_slice.lock() = None;
        *self.watcher.lock() = None;
        if let Some(identity) = self.watcher_runtime_identity.lock().take() {
            crate::lsp::manager::invalidate_applicability_walk(
                &identity.root,
                crate::lsp::manager::InvalidationCause::WatcherRemoved,
            );
        }
        runtime
    }

    /// Stop the watcher runtime without waiting on its OS thread. Shutdown and
    /// the bounded join run on a detached reaper so configure and transport
    /// loops never wait on FSEvents or inotify teardown.
    pub fn stop_watcher_runtime(&self) {
        if let Some(runtime) = self.take_watcher_runtime() {
            Self::spawn_watcher_shutdown(Arc::clone(&self.app), self.watcher_root_path(), runtime);
        }
    }

    /// Request watcher shutdown without joining on the executor lane.
    pub fn stop_watcher_runtime_in_background(&self) {
        self.stop_watcher_runtime();
    }

    fn try_stop_watcher_runtime_in_background(&self) -> bool {
        let taken = (|| {
            let _runtime = self.watcher_runtime_lock.try_lock()?;
            let mut thread = self.watcher_thread.try_lock()?;
            let mut rx = self.watcher_rx.try_lock()?;
            let mut slice = self.watcher_drain_slice.try_lock()?;
            let mut watcher = self.watcher.try_lock()?;
            let mut identity = self.watcher_runtime_identity.try_lock()?;
            *rx = None;
            *slice = None;
            *watcher = None;
            Some((thread.take(), identity.take()))
        })();
        let Some((runtime, identity)) = taken else {
            return false;
        };
        if let Some(identity) = identity {
            crate::lsp::manager::invalidate_applicability_walk(
                &identity.root,
                crate::lsp::manager::InvalidationCause::WatcherRemoved,
            );
        }
        if let Some(runtime) = runtime {
            Self::spawn_watcher_shutdown(Arc::clone(&self.app), self.watcher_root_path(), runtime);
        }
        true
    }

    /// Remove a watcher runtime whose OS thread already exited (backend
    /// failure while the root was unbound and drains were suppressed).
    /// Returns true when a finished corpse was actually removed so the caller
    /// can apply watcher-gap invalidation exactly once.
    pub(crate) fn take_finished_watcher_runtime(&self) -> bool {
        let runtime = {
            let _runtime_guard = self.watcher_runtime_lock.lock();
            let finished = self
                .watcher_thread
                .lock()
                .as_ref()
                .is_some_and(|runtime| runtime.is_finished());
            if !finished {
                return false;
            }
            let runtime = self.watcher_thread.lock().take();
            *self.watcher_rx.lock() = None;
            *self.watcher_drain_slice.lock() = None;
            *self.watcher.lock() = None;
            if let Some(identity) = self.watcher_runtime_identity.lock().take() {
                crate::lsp::manager::invalidate_applicability_walk(
                    &identity.root,
                    crate::lsp::manager::InvalidationCause::WatcherRemoved,
                );
            }
            runtime
        };
        if let Some(runtime) = runtime {
            Self::spawn_watcher_shutdown(Arc::clone(&self.app), self.watcher_root_path(), runtime);
        }
        true
    }

    /// Process-scoped watcher count used by maintenance diagnostics and
    /// regression tests. A runtime remains counted until its thread exits.
    pub fn watcher_registry_count(&self) -> usize {
        self.app.watcher_count()
    }

    pub(crate) fn watcher_runtime_active(&self) -> bool {
        let _runtime_guard = self.watcher_runtime_lock.lock();
        // A finished thread is a dead runtime even while its handle is still
        // installed (the backend can fail while drains are suppressed for an
        // unbound root, leaving the queued error undrained). Treating it as
        // active would block watcher restoration on rebind.
        let thread_live = self
            .watcher_thread
            .lock()
            .as_ref()
            .is_some_and(|runtime| !runtime.is_finished());
        thread_live && self.watcher_rx.lock().is_some()
    }

    /// Return whether artifact eviction would discard work that still needs a
    /// live handle. Callers use this as the single safety gate before clearing
    /// resident stores and inspect caches.
    pub fn artifact_eviction_blocked(&self) -> bool {
        self.artifact_eviction_blocker().is_some()
    }

    /// The first condition that makes artifact eviction unsafe, or `None` when
    /// eviction may proceed. Conditions are checked in a fixed order.
    pub(crate) fn artifact_eviction_blocker(&self) -> Option<ArtifactEvictionBlocker> {
        if self.standing_artifact_exempt.load(Ordering::Acquire) {
            return Some(ArtifactEvictionBlocker::StandingRoot);
        }
        let semantic_refresh_in_flight = match &*self
            .semantic_index_status
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
        {
            SemanticIndexStatus::Building { .. } => true,
            SemanticIndexStatus::Ready { refreshing, .. } => !refreshing.is_empty(),
            SemanticIndexStatus::Disabled | SemanticIndexStatus::Failed(_) => false,
        };
        if crate::runtime_drain::any_build_in_flight(self) {
            return Some(ArtifactEvictionBlocker::BuildInFlight);
        }
        if semantic_refresh_in_flight {
            return Some(ArtifactEvictionBlocker::SemanticRefresh);
        }
        if self.inspect_manager.tier2_any_in_flight() {
            return Some(ArtifactEvictionBlocker::InspectTier2);
        }
        if !self.bash_background.running_tasks().is_empty() {
            return Some(ArtifactEvictionBlocker::BashRunning);
        }
        if !self.pending_tier2_paths.lock().is_empty()
            || !self.pending_semantic_index_paths.lock().is_empty()
            || *self.pending_semantic_corpus_refresh.lock()
        {
            return Some(ArtifactEvictionBlocker::PendingReconciliation);
        }

        None
    }

    /// Persist the resident search-index delta of an unbound root so the idle
    /// reaper can evict it, and drop inspect completions that arrived after
    /// the unbind (nothing drains them while the root is unbound, and each one
    /// keeps its Tier-2 job counted as in flight).
    ///
    /// This is the only artifact work an unbound root runs. It does nothing
    /// once the root is bound again: the rebind owns the root's artifacts from
    /// that point, and configure persists the delta itself. A rebind that lands
    /// after the check below only lets an ordinary delta write finish, which is
    /// harmless for a bound root. Returns whether a delta was written.
    pub(crate) fn persist_unbound_artifacts_for_teardown(&self) -> bool {
        if !self.subc_unbound_quiesced() {
            return false;
        }
        let _ = self.inspect_manager.discard_completions();
        #[cfg(test)]
        wait_on_unbound_teardown_persist_gate_for_test(self.canonical_cache_root_opt().as_deref());
        // The index store publishes as it goes, so an unbound root has no
        // in-memory index delta to persist.
        false
    }

    /// Drop idle root-scoped artifact handles. Persistent data remains on disk;
    /// artifact-backed query paths schedule a background reload on first use.
    /// Returns false when an active build, bash task, inspect scan, or pending
    /// disk update makes eviction unsafe.
    pub fn evict_idle_artifacts(&self) -> bool {
        if self.artifact_eviction_blocked() {
            return false;
        }

        self.search_exact_memo.clear();

        self.semantic_index
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        self.clear_semantic_refresh_worker();
        *self.semantic_embedding_model.lock() = None;
        self.inspect_manager.evict_idle_caches();
        self.reset_symbol_cache();
        self.clear_tsconfig_membership_cache();
        self.backup.lock().release_memory();
        true
    }

    /// [`Self::artifact_eviction_blocker`] for the subc frame loop: every lock
    /// it reads is try-locked, and a contended one answers
    /// [`ArtifactEvictionBlocker::LockContended`] instead of waiting. The idle
    /// reaper takes the pending reconciliation sets before it asks, so it
    /// passes `include_pending = false` for that check.
    pub(crate) fn try_artifact_eviction_blocker(
        &self,
        include_pending: bool,
    ) -> Option<ArtifactEvictionBlocker> {
        self.try_artifact_eviction_blocker_for_root(include_pending, None, false)
    }

    pub(crate) fn try_artifact_eviction_blocker_for_deleted_root(
        &self,
        include_pending: bool,
        root: &crate::path_identity::ProjectRootId,
    ) -> Option<ArtifactEvictionBlocker> {
        self.try_artifact_eviction_blocker_for_root(include_pending, Some(root), false)
    }

    fn try_artifact_eviction_blocker_for_root(
        &self,
        include_pending: bool,
        deleted_root: Option<&crate::path_identity::ProjectRootId>,
        allow_standing: bool,
    ) -> Option<ArtifactEvictionBlocker> {
        use ArtifactEvictionBlocker as Blocker;
        if !allow_standing && self.standing_artifact_exempt.load(Ordering::Acquire) {
            return Some(Blocker::StandingRoot);
        }
        let semantic_refresh_in_flight = match try_read_unpoisoned(&self.semantic_index_status) {
            Some(status) => match &*status {
                SemanticIndexStatus::Building { .. } => true,
                SemanticIndexStatus::Ready { refreshing, .. } => !refreshing.is_empty(),
                SemanticIndexStatus::Disabled | SemanticIndexStatus::Failed(_) => false,
            },
            None => return Some(Blocker::LockContended),
        };
        match crate::runtime_drain::try_any_build_in_flight(self) {
            None => return Some(Blocker::LockContended),
            Some(true) => return Some(Blocker::BuildInFlight),
            Some(false) => {}
        }
        if semantic_refresh_in_flight {
            return Some(Blocker::SemanticRefresh);
        }
        match self.inspect_manager.try_tier2_any_in_flight() {
            None => return Some(Blocker::LockContended),
            Some(true) => return Some(Blocker::InspectTier2),
            Some(false) => {}
        }
        let bash_busy = match deleted_root {
            Some(root) => self
                .bash_background
                .try_has_reclaimable_tasks_for_root(root),
            None => self.bash_background.try_has_running_tasks(),
        };
        match bash_busy {
            None => return Some(Blocker::LockContended),
            Some(true) => return Some(Blocker::BashRunning),
            Some(false) => {}
        }
        if include_pending {
            match self.try_pending_reconciliation_present() {
                None => return Some(Blocker::LockContended),
                Some(true) => return Some(Blocker::PendingReconciliation),
                Some(false) => {}
            }
        }
        if self.shared_artifacts_read_only() {
            return None;
        }
        None
    }

    fn try_pending_reconciliation_present(&self) -> Option<bool> {
        Some(
            !self.pending_tier2_paths.try_lock()?.is_empty()
                || !self.pending_semantic_index_paths.try_lock()?.is_empty()
                || *self.pending_semantic_corpus_refresh.try_lock()?,
        )
    }

    /// Take the retained pending reconciliation state for a transactional
    /// teardown, without waiting: `None`, with nothing taken, when any of the
    /// pending sets is locked. The caller commits the disposal by dropping the
    /// returned state after eviction succeeds, or restores it with
    /// [`Self::restore_pending_reconciliation_state_without_waiting`] when
    /// eviction is blocked by a secondary blocker (running bash, in-flight
    /// builds): the paths are the only repair record for consumed watcher
    /// events, and the root may rebind before the next reap attempt.
    pub(crate) fn try_take_pending_reconciliation_state(
        &self,
    ) -> Option<PendingReconciliationState> {
        let mut tier2 = self.pending_tier2_paths.try_lock()?;
        let mut semantic = self.pending_semantic_index_paths.try_lock()?;
        let mut corpus_refresh = self.pending_semantic_corpus_refresh.try_lock()?;
        Some(PendingReconciliationState {
            tier2: std::mem::take(&mut *tier2),
            semantic: std::mem::take(&mut *semantic),
            corpus_refresh: std::mem::take(&mut *corpus_refresh),
        })
    }

    /// Put taken pending sets back without waiting on the caller's thread.
    /// The sets are the only record of watcher events already consumed, so
    /// they are never dropped: when a set is locked, a short-lived thread
    /// restores them instead.
    pub(crate) fn restore_pending_reconciliation_state_without_waiting(
        self: &Arc<Self>,
        state: PendingReconciliationState,
    ) {
        let guards = (|| {
            Some((
                self.pending_tier2_paths.try_lock()?,
                self.pending_semantic_index_paths.try_lock()?,
                self.pending_semantic_corpus_refresh.try_lock()?,
            ))
        })();
        match guards {
            Some((mut tier2, mut semantic, mut corpus_refresh)) => {
                restore_pending_paths(&mut tier2, state.tier2);
                restore_pending_paths(&mut semantic, state.semantic);
                if state.corpus_refresh {
                    *corpus_refresh = true;
                }
            }
            None => {
                let ctx = Arc::clone(self);
                std::thread::spawn(move || ctx.restore_pending_reconciliation_state(state));
            }
        }
    }

    /// [`Self::evict_idle_artifacts`] for the subc frame loop: the eviction
    /// gate and the resident-handle write locks are all try-locked, and
    /// nothing is evicted unless every one of them was free. Returns false
    /// when eviction is unsafe or a lock was contended; the reaper retries on
    /// its next sweep.
    pub(crate) fn try_evict_idle_artifacts(&self) -> bool {
        self.try_evict_idle_artifacts_for_root(None, false)
    }

    /// A standing checkout's views can age out too. The caller must exclude
    /// in-flight actor work before releasing its resident registrations.
    pub(crate) fn try_release_idle_checkout_views(&self) -> bool {
        self.try_evict_idle_artifacts_for_root(None, true)
    }

    /// Deleted-root reclamation uses the same root-scoped task gate both before
    /// and during eviction. Artifact handles belong to this root, so only its
    /// tasks block reclamation of those handles.
    pub(crate) fn try_evict_idle_artifacts_for_deleted_root(
        &self,
        root: &crate::path_identity::ProjectRootId,
    ) -> bool {
        self.try_evict_idle_artifacts_for_root(Some(root), false)
    }

    fn try_evict_idle_artifacts_for_root(
        &self,
        deleted_root: Option<&crate::path_identity::ProjectRootId>,
        allow_standing: bool,
    ) -> bool {
        if self
            .try_artifact_eviction_blocker_for_root(true, deleted_root, allow_standing)
            .is_some()
        {
            return false;
        }
        let Some(mut semantic_index) = try_write_unpoisoned(&self.semantic_index) else {
            return false;
        };
        let Some(mut backup) = self.backup.try_lock() else {
            return false;
        };
        let Some(mut embedding_model) = self.semantic_embedding_model.try_lock() else {
            return false;
        };
        let Some(mut refresh_events) = self.semantic_refresh_event_rx.try_lock() else {
            return false;
        };
        let Some(mut refresh_requests) = self.semantic_refresh_tx.try_lock() else {
            return false;
        };
        let Some(mut refresh_worker) = self.semantic_refresh_worker.try_lock() else {
            return false;
        };
        // The data layer's checkout keeps its published state on disk; what it
        // holds in memory (decoded call-graph slices, the update engine, the
        // text overlay's index) is rebuilt from that state at its next use.
        let Some(mut data_checkout) = self.data_checkout.try_lock() else {
            return false;
        };
        let Some(mut data_external) = self.data_external.try_lock() else {
            return false;
        };
        if !self.search_exact_memo.try_clear_idle() {
            return false;
        }
        semantic_index.take();
        backup.release_memory();
        let released = (
            embedding_model.take(),
            refresh_events.take(),
            refresh_requests.take(),
            refresh_worker.take(),
            data_checkout.take(),
            std::mem::take(&mut *data_external),
        );
        self.semantic_refresh_epoch.fetch_add(1, Ordering::SeqCst);
        self.semantic_refresh_build_epoch.store(0, Ordering::SeqCst);
        self.invalidate_semantic_refresh_probe();
        drop((
            embedding_model,
            refresh_events,
            refresh_requests,
            refresh_worker,
            data_checkout,
            data_external,
        ));
        // Native session destruction can wait for ORT. Keep it, and worker
        // channel teardown, off the daemon's frame loop.
        let _ = std::thread::Builder::new()
            .name("aft-embedding-release".into())
            .spawn(move || drop(released));
        drop((semantic_index, backup));
        self.inspect_manager.evict_idle_caches();
        self.reset_symbol_cache();
        self.clear_tsconfig_membership_cache();
        true
    }

    /// Test seam for the serialized real-watcher integration suite. Production
    /// callers cannot trigger it without the explicit test-only environment flag.
    #[doc(hidden)]
    pub fn force_idle_teardown_for_test(self: &Arc<Self>) -> bool {
        if std::env::var("AFT_TEST_ALLOW_FORCE_IDLE_REAP").as_deref() != Ok("1") {
            return false;
        }
        if !self.evict_idle_artifacts() {
            return false;
        }
        self.stop_watcher_runtime_in_background();
        self.invalidate_artifacts_after_watcher_gap();
        true
    }

    /// Release resources that can be recreated by an equivalent later bind.
    /// LSP shutdown can wait on child processes, so all work stays off the
    /// executor and subc frame loops.
    pub(crate) fn release_idle_reopenable_resources_in_background(
        self: &Arc<Self>,
    ) -> std::thread::JoinHandle<()> {
        let ctx = Arc::clone(self);
        std::thread::spawn(move || {
            #[cfg(all(test, windows))]
            Self::describe_teardown_thread_for_test("aft-idle-resource-release");
            if !ctx.subc_unbound_quiesced() {
                return;
            }
            {
                let mut lsp = ctx.lsp_manager.lock();
                if !ctx.subc_unbound_quiesced() {
                    return;
                }
                lsp.shutdown_all();
            }
            let released = ctx.subc_lifecycle.run_if_unbound(|| {
                ctx.bash_background.clear_db_pool();
                ctx.backup.lock().clear_db_pool();
                // The root's own config file watches each hold a thread (and on
                // macOS an FSEvents run loop). Nothing reloads an unbound
                // root's config; a rebind's configure starts them again.
                let watches = ctx.config_live.take_watches();
                // A query embedding model is rebuilt on the next semantic
                // query. A local model holds its ONNX session and a query
                // thread, which an evicted root has no use for. A model in
                // use is left alone rather than waited for.
                let model = ctx
                    .semantic_embedding_model
                    .try_lock()
                    .and_then(|mut model| model.take());
                // Background task output is compressed through the shared
                // filter handle, which cannot load a released registry. A
                // root that still holds task records keeps its filters, so a
                // later render of that output compresses exactly as it would
                // have without the eviction.
                let deferred = ctx.bash_background.has_task_records();
                ctx.filter_release_deferred
                    .store(deferred, Ordering::Release);
                let filters = (!deferred).then(|| ctx.take_filter_registry());
                (watches, model, filters)
            });
            // Dropped after the lifecycle check releases, so their shutdown
            // never holds up a rebind.
            drop(released);
        })
    }

    /// Run the idle release again for an evicted root whose filters were kept
    /// because it still held background task records, once the last record
    /// is gone. The idle reaper calls this on every sweep for evicted,
    /// unbound roots: an evicted root is not reaped again, so nothing else
    /// would retry. Returns whether a release was started. Waits on nothing;
    /// a locked task map means "not yet".
    pub(crate) fn retry_deferred_idle_release(self: &Arc<Self>) -> bool {
        if !self.filter_release_deferred.load(Ordering::Acquire)
            || self.bash_background.try_has_task_records() != Some(false)
        {
            return false;
        }
        // Cleared before the release starts, so the next sweep does not start
        // a second one; the release sets it again if records came back.
        if !self.filter_release_deferred.swap(false, Ordering::AcqRel) {
            return false;
        }
        self.release_idle_reopenable_resources_in_background();
        true
    }

    /// Final cleanup for an actor whose project directory no longer exists.
    /// The executor invokes this only after proving the actor has no queued or
    /// running jobs, and always from a detached teardown thread.
    pub(crate) fn teardown_deleted_root(&self) {
        self.inspect_manager.cancel_root_work();
        self.bash_background.detach();
        self.bash_background.clear_db_pool();
        self.backup.lock().clear_db_pool();
        self.lsp_manager.lock().shutdown_all();
    }

    /// Access the LSP manager.
    pub fn lsp(&self) -> parking_lot::MutexGuard<'_, LspManager> {
        self.lsp_manager.lock()
    }

    /// Keep Cargo-driven rust-analyzer checks off for this root from the first
    /// worker call on (see [`LspManager::disable_rust_cargo_for_worker`]).
    ///
    /// Every worker tool call asks for this, so the flag is read without the
    /// manager lock first: once it is set the call returns at once instead of
    /// waiting behind a language-server start or a diagnostics request that
    /// holds the lock. After one call sets the flag, later calls never take
    /// the lock; only calls racing that first one may still contend for it.
    pub fn disable_rust_cargo_for_worker(&self) {
        if self
            .lsp_rust_worker_policy
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return;
        }
        self.lsp_manager.lock().disable_rust_cargo_for_worker();
    }

    /// The LSP manager if no other thread holds it right now. For callers on
    /// the subc frame loop, which must not wait behind a server start or a
    /// diagnostics request.
    pub fn try_lsp(&self) -> Option<parking_lot::MutexGuard<'_, LspManager>> {
        self.lsp_manager.try_lock()
    }

    /// Start one inspect producer, holding the LSP manager lock only to
    /// reserve the server and to publish it; the spawn and `initialize`
    /// handshake run unlocked (see
    /// [`crate::lsp::manager::start_applicable_server_unlocked`]).
    pub fn lsp_start_applicable_server_until(
        &self,
        snapshot: &crate::lsp::manager::ApplicableServerSnapshot,
        server: &crate::lsp::roots::ServerKey,
        config: &Config,
        deadline: Instant,
    ) -> crate::lsp::manager::ApplicableServerStartOutcomes {
        crate::lsp::manager::start_applicable_server_unlocked(
            &self.lsp_manager,
            snapshot,
            server,
            config,
            deadline,
        )
    }

    /// Make a running rust-analyzer reload its workspace when its Cargo
    /// manifests, lockfile, toolchain file, or Cargo config changed since it
    /// last loaded them (see
    /// [`crate::lsp::manager::reload_rust_workspace_if_manifests_changed`]).
    pub fn lsp_reload_rust_workspace_if_manifests_changed(
        &self,
        server: &crate::lsp::roots::ServerKey,
        scope_roots: &[PathBuf],
    ) -> bool {
        crate::lsp::manager::reload_rust_workspace_if_manifests_changed(
            &self.lsp_manager,
            server,
            scope_roots,
        )
    }

    /// Notify LSP servers that a file was written.
    /// Call this after write_format_validate in command handlers.
    /// A busy manager queues the change rather than dropping it.
    pub fn lsp_notify_file_changed(&self, file_path: &Path, content: &str) {
        let config = self.config();
        crate::lsp::pending_changes::send_or_queue_lsp_change(
            &self.lsp_manager,
            &self.lsp_pending_change_slot,
            Arc::clone(&config),
            |lsp| {
                if let Err(e) = lsp.notify_file_changed_if_running(file_path, content, &config) {
                    crate::slog_warn!("sync error for {}: {}", file_path.display(), e);
                }
            },
            |pending| pending.queue_document(file_path, false),
        );
    }

    /// Whether document or configuration-file changes are still queued for
    /// the LSP manager.
    #[doc(hidden)]
    pub fn lsp_pending_changes_for_test(&self) -> bool {
        self.lsp_pending_change_slot.lock().is_some()
    }

    /// Forward paths the project file watcher saw change to the language
    /// servers that registered for them (see
    /// [`LspManager::forward_watcher_file_events`]), and have rust-analyzer
    /// reload its workspace when a Cargo manifest among them changed.
    ///
    /// The watcher reports paths without the kind of change, so a path that
    /// exists is reported as changed and a missing one as deleted, as AFT's
    /// own config-file notifications do. rust-analyzer decides between
    /// created and modified from its own file set, whatever the event says.
    pub fn lsp_forward_watcher_file_events(&self, paths: &[PathBuf]) {
        if paths.is_empty() {
            return;
        }
        self.lsp_note_watcher_batch(paths, false);
        self.lsp_forward_or_queue_watcher_changes(paths, false);
    }

    /// Check the applicability cache when a watcher batch enters the drain,
    /// before its paths are forwarded to language servers in later drain slices.
    /// `content_only` is true only if every event for every path was a data
    /// modification; callers with paths alone must pass false to allow creation.
    pub(crate) fn lsp_note_watcher_batch(&self, paths: &[PathBuf], content_only: bool) {
        let config = self.config();
        if let Some(root) = &config.project_root {
            if crate::lsp::manager::invalidate_applicability_batch(
                root,
                paths,
                content_only,
                &config,
            ) {
                self.refresh_project_applicability(config, false);
            }
        }
    }

    pub(crate) fn lsp_forward_classified_watcher_file_events(&self, paths: &[PathBuf]) {
        #[cfg(test)]
        if let Some(batches) = self.lsp_classified_watcher_forwards.lock().as_mut() {
            batches.push(paths.to_vec());
        }
        self.lsp_forward_or_queue_watcher_changes(paths, false);
    }

    #[cfg(test)]
    pub(crate) fn capture_classified_watcher_forwards_for_test(&self) {
        *self.lsp_classified_watcher_forwards.lock() = Some(Vec::new());
    }

    #[cfg(test)]
    pub(crate) fn take_classified_watcher_forwards_for_test(&self) -> Vec<Vec<PathBuf>> {
        std::mem::take(
            self.lsp_classified_watcher_forwards
                .lock()
                .as_mut()
                .expect("classified watcher forward capture must be enabled"),
        )
    }

    pub(crate) fn lsp_note_watcher_ignore_change(&self) {
        self.invalidate_project_applicability(
            crate::lsp::manager::InvalidationCause::SelectionInput,
        );
    }

    /// After the file watcher lost events (an overflow that needs a rescan),
    /// ask every running rust-analyzer to reload its workspace if a Cargo
    /// manifest changed since its last load. Other servers get no per-file
    /// events: the lost paths are unknown, and a flood of guesses would cost
    /// them more than a missed notification for files AFT resyncs when it
    /// next opens or edits them.
    pub fn lsp_reload_rust_workspaces_after_lost_watcher_events(&self) {
        self.invalidate_project_applicability(
            crate::lsp::manager::InvalidationCause::OverflowLostEvents,
        );
        self.lsp_forward_or_queue_watcher_changes(&[], true);
    }

    fn invalidate_project_applicability(&self, cause: crate::lsp::manager::InvalidationCause) {
        let config = self.config();
        if let Some(root) = &config.project_root {
            crate::lsp::manager::invalidate_applicability_walk(root, cause);
            self.refresh_project_applicability(config, false);
        }
    }

    /// Deliver watcher changes to the LSP manager without making the drain
    /// wait for its lock.
    ///
    /// An uncontended lock forwards them here. A contended one queues them in
    /// a single per-context slot served by one helper thread that waits for
    /// the lock: while that helper exists, later drains merge into its slot
    /// (deduplicated and capped, see [`crate::lsp::manager::WatcherForwardBacklog`])
    /// and start nothing. Dropping the changes instead would leave servers
    /// on the old files (and rust-analyzer on the old workspace) until some
    /// later change; a thread per contended drain would pile up without
    /// bound while the lock stays held, for example during a server start.
    fn lsp_forward_or_queue_watcher_changes(&self, paths: &[PathBuf], reload_all_rust: bool) {
        use crate::lsp::manager::WatcherForwardBacklog;
        let markers = self.custom_lsp_root_markers();
        let stamps = crate::lsp::manager::capture_watched_file_stamps(paths);
        {
            // A queued backlog goes first, so newer changes join it rather
            // than overtake it on the fast path.
            let mut slot = self.lsp_watcher_forward_slot.lock();
            if let Some(backlog) = slot.as_mut() {
                backlog.merge_with_stamps(paths, reload_all_rust, markers, &stamps);
                return;
            }
        }
        if let Some(mut lsp) = self.lsp_manager.try_lock() {
            let mut backlog = WatcherForwardBacklog::default();
            backlog.merge_with_stamps(paths, reload_all_rust, markers, &stamps);
            let reloads = backlog.apply(&mut lsp);
            drop(lsp);
            crate::lsp::manager::spawn_watcher_rust_workspace_reloads(&self.lsp_manager, reloads);
            return;
        }
        let mut slot = self.lsp_watcher_forward_slot.lock();
        if let Some(backlog) = slot.as_mut() {
            backlog.merge_with_stamps(paths, reload_all_rust, markers, &stamps);
            return;
        }
        let mut backlog = WatcherForwardBacklog::default();
        backlog.merge_with_stamps(paths, reload_all_rust, markers, &stamps);
        *slot = Some(backlog);
        drop(slot);
        let manager = Arc::clone(&self.lsp_manager);
        let forward_slot = Arc::clone(&self.lsp_watcher_forward_slot);
        let pause = Arc::clone(&self.lsp_watcher_forward_pause);
        match std::thread::Builder::new()
            .name("aft-lsp-watched-files".into())
            .spawn(move || {
                crate::lsp::manager::run_watcher_forward_helper(&manager, &forward_slot, &pause)
            }) {
            Ok(_) => {
                self.lsp_watcher_forward_helpers_spawned
                    .fetch_add(1, Ordering::Relaxed);
            }
            Err(error) => {
                // Free the slot so a later drain can try again.
                self.lsp_watcher_forward_slot.lock().take();
                crate::slog_warn!("could not reach LSP servers after a watcher change: {error}");
            }
        }
    }

    /// Forward the watcher changes still queued for the LSP manager (see
    /// [`Self::lsp_forward_or_queue_watcher_changes`]) now, waiting for its
    /// lock, instead of leaving them to the helper thread.
    ///
    /// Diagnostics readers call this first. A change that is still queued has
    /// not reached the language servers: rust-analyzer has neither heard of
    /// it nor been asked for the `cargo check` it needs, so nothing marks its
    /// published results as describing the files before the change, and a
    /// reader would certify them as current. Once forwarded, the check the
    /// change asked for counts as running until it reports.
    ///
    /// The backlog is taken while holding the manager lock, the order the
    /// helper uses, so changes reach the servers in the order they were
    /// queued. The slot itself stays in place: the helper still owns it,
    /// finds it empty, and clears it.
    pub(crate) fn lsp_forward_queued_watcher_changes(&self) {
        let reloads = {
            let mut lsp = self.lsp_manager.lock();
            let Some(backlog) = self
                .lsp_watcher_forward_slot
                .lock()
                .as_mut()
                .map(std::mem::take)
            else {
                return;
            };
            backlog.apply(&mut lsp)
        };
        crate::lsp::manager::spawn_watcher_rust_workspace_reloads(&self.lsp_manager, reloads);
    }

    /// Keep the helper thread that forwards queued watcher changes from
    /// forwarding anything until the returned guard is dropped, so a test
    /// can act while a change waits in the queue.
    #[doc(hidden)]
    pub fn pause_lsp_watcher_forwarding_for_test(&self) -> parking_lot::MutexGuard<'_, ()> {
        self.lsp_watcher_forward_pause.lock()
    }

    /// How many helper threads have waited for the LSP manager lock on behalf
    /// of contended watcher drains.
    #[doc(hidden)]
    pub fn lsp_watcher_forward_helpers_spawned_for_test(&self) -> usize {
        self.lsp_watcher_forward_helpers_spawned
            .load(Ordering::Relaxed)
    }

    /// Whether watcher changes are still queued for the LSP manager.
    #[doc(hidden)]
    pub fn lsp_watcher_forward_pending_for_test(&self) -> bool {
        self.lsp_watcher_forward_slot.lock().is_some()
    }

    /// Drop cached LSP diagnostics for a deleted/renamed-away file so its
    /// errors/warnings don't linger in the warm set (no server republishes for
    /// a vanished path), keeping the status bar and `aft_inspect` honest.
    /// Returns true if any entry was removed. Best-effort: a contended borrow is
    /// skipped silently (the watcher drain retries on subsequent events).
    pub fn lsp_clear_diagnostics_for_file(&self, file_path: &Path) -> bool {
        if let Some(mut lsp) = self.lsp_manager.try_lock() {
            lsp.clear_diagnostics_for_file(file_path)
        } else {
            false
        }
    }

    /// Mark diagnostics stale for a file changed outside AFT's text-sync path.
    /// Best-effort: a contended LSP lock is skipped and the next watcher event
    /// or scoped diagnostics pull can reconcile the file.
    pub fn lsp_mark_diagnostics_stale_for_file(&self, file_path: &Path) -> StaleDiagnosticsMark {
        if let Some(mut lsp) = self.lsp_manager.try_lock() {
            lsp.mark_diagnostics_stale_for_file(file_path)
        } else {
            StaleDiagnosticsMark::default()
        }
    }

    /// Resync a watcher-stale diagnosed file with the active LSP server.
    ///
    /// `workspace/didChangeWatchedFiles` tells servers that the filesystem
    /// changed, but it does not update an already-open document's in-memory text.
    /// Sending the normal didOpen/didChange path gives push-only servers a chance
    /// to publish fresh diagnostics and keeps pull-capable servers' document state
    /// current for the next diagnostic request.
    pub fn lsp_resync_changed_file_for_diagnostics(&self, file_path: &Path) -> bool {
        if !file_path.is_file() {
            return false;
        }

        let content = match std::fs::read_to_string(file_path) {
            Ok(content) => content,
            Err(err) => {
                crate::slog_warn!(
                    "skipping LSP resync for {} after external edit: {}",
                    file_path.display(),
                    err
                );
                return false;
            }
        };

        let config = self.config();
        let mut sent = true;
        // A busy manager queues the resync (delivered with the file's
        // contents at that time) instead of dropping it.
        crate::lsp::pending_changes::send_or_queue_lsp_change(
            &self.lsp_manager,
            &self.lsp_pending_change_slot,
            Arc::clone(&config),
            |lsp| {
                if let Err(err) = lsp.notify_file_changed(file_path, &content, &config) {
                    crate::slog_warn!(
                        "LSP resync failed for {} after external edit: {}",
                        file_path.display(),
                        err
                    );
                    sent = false;
                }
            },
            |pending| pending.queue_document(file_path, true),
        );
        sent
    }

    /// Notify LSP and optionally wait for diagnostics.
    ///
    /// Call this after `write_format_validate` when the request has `"diagnostics": true`.
    /// Ensures the matching server is running, sends didOpen/didChange, requests
    /// pull diagnostics when supported, and otherwise waits briefly for
    /// publishDiagnostics before returning diagnostics for the file.
    ///
    /// Pre-edit cached diagnostics are never returned: only entries proven against
    /// the post-edit document version are authoritative.
    pub fn lsp_notify_and_collect_diagnostics(
        &self,
        file_path: &Path,
        content: &str,
        timeout: std::time::Duration,
    ) -> crate::lsp::manager::PostEditWaitOutcome {
        let config = self.config();
        let timeout = timeout.min(Duration::from_secs(10));
        let pending = crate::lsp::manager::PostEditWaitOutcome {
            pending_servers: crate::lsp::manager::expected_server_keys_for_file(file_path, &config),
            ..Default::default()
        };
        if pending.pending_servers.is_empty() {
            return pending;
        }
        if timeout.is_zero()
            || self
                .lsp_post_edit_worker_running
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            crate::lsp::pending_changes::queue_lsp_change(
                &self.lsp_manager,
                &self.lsp_pending_change_slot,
                config,
                |pending| pending.queue_document(file_path, !timeout.is_zero()),
            );
            return pending;
        }

        // The executor's writer slot waits only for this fixed budget. Cold
        // initialization and manager reacquisition can otherwise extend a
        // diagnostics wait even when the channel wait itself is lock-free.
        struct RunningCollector(Arc<AtomicBool>);
        impl Drop for RunningCollector {
            fn drop(&mut self) {
                self.0.store(false, Ordering::Release);
            }
        }
        let running = RunningCollector(Arc::clone(&self.lsp_post_edit_worker_running));
        let manager = Arc::clone(&self.lsp_manager);
        let slot = Arc::clone(&self.lsp_pending_change_slot);
        let file_path = file_path.to_path_buf();
        let content = content.to_string();
        let deadline = Instant::now() + timeout;
        let (tx, rx) = crossbeam_channel::bounded(1);
        if let Err(err) = std::thread::Builder::new()
            .name("aft-lsp-post-edit".into())
            .spawn(move || {
                let outcome = Self::lsp_collect_post_edit_diagnostics(
                    &manager, &slot, config, &file_path, &content, deadline,
                );
                // Release before sending: a sequential caller must not mistake
                // the completed collector for an overlapping edit.
                drop(running);
                let _ = tx.send(outcome);
            })
        {
            crate::slog_warn!("could not start post-edit diagnostics collector: {err}");
            return pending;
        }
        rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap_or(pending)
    }

    #[cfg(test)]
    pub(crate) fn lsp_post_edit_worker_running_for_test(&self) -> bool {
        self.lsp_post_edit_worker_running.load(Ordering::Acquire)
    }

    fn lsp_collect_post_edit_diagnostics(
        manager: &Arc<parking_lot::Mutex<LspManager>>,
        slot: &crate::lsp::pending_changes::PendingLspChangeSlot,
        config: Arc<Config>,
        file_path: &Path,
        content: &str,
        deadline: Instant,
    ) -> crate::lsp::manager::PostEditWaitOutcome {
        crate::lsp::manager::start_servers_for_file_unlocked(|| manager.lock(), file_path, &config);
        let locked = manager.try_lock_for(deadline.saturating_duration_since(Instant::now()));
        if locked.is_none() || Instant::now() >= deadline {
            drop(locked);
            // Do not send captured contents after the caller timed out: a
            // newer edit may already be on disk. The backlog reads it afresh.
            crate::lsp::pending_changes::queue_lsp_change(
                manager,
                slot,
                Arc::clone(&config),
                |pending| pending.queue_document(file_path, true),
            );
            return crate::lsp::manager::PostEditWaitOutcome {
                pending_servers: crate::lsp::manager::expected_server_keys_for_file(
                    file_path, &config,
                ),
                ..Default::default()
            };
        }
        let mut lsp = locked.expect("manager lock acquired before deadline");

        // Clear any queued notifications before this write so the wait loop only
        // observes diagnostics triggered by the current change.
        lsp.drain_events();

        // Snapshot per-server epochs and document versions BEFORE sending
        // didChange so the wait loop can prove freshness without accepting
        // stale pre-edit publishes that arrived late.
        let pre_snapshot = lsp.snapshot_pre_edit_state(file_path);

        // An explicit diagnostics request still starts matching servers only when
        // needed. Record the document version sent to each server so completed results
        // stay tied to the server and version that produced them.
        let expected_versions = match lsp.notify_file_changed_versioned(file_path, content, &config)
        {
            Ok(v) => v,
            Err(e) => {
                crate::slog_warn!("sync error for {}: {}", file_path.display(), e);
                let pending_servers =
                    crate::lsp::manager::expected_server_keys_for_file(file_path, &config);
                let unresponsive_servers = pending_servers
                    .iter()
                    .filter(|key| lsp.producer_failure(key) == Some("server not responding"))
                    .cloned()
                    .collect();
                return crate::lsp::manager::PostEditWaitOutcome {
                    pending_servers,
                    unresponsive_servers,
                    ..Default::default()
                };
            }
        };

        // No server matched this file — return an empty outcome that's
        // honestly `complete: true` (nothing to wait for).
        if expected_versions.is_empty() {
            return crate::lsp::manager::PostEditWaitOutcome::default();
        }

        // Some LSP 3.17 servers disable publishDiagnostics as soon as the client
        // advertises pull support. Pull before parking so edits work for those
        // servers while push-only servers still use the event-driven wait below.
        // The manager lock is released while servers work on the pull; the
        // wait registered below re-checks the store, so a publish another
        // drain consumed in the meantime is not missed.
        drop(lsp);
        if let Err(err) = crate::lsp::manager::pull_file_diagnostics_unlocked(
            || manager.lock(),
            file_path,
            &config,
            Some(deadline.saturating_duration_since(Instant::now())),
        ) {
            crate::slog_warn!(
                "post-edit LSP diagnostic pull failed for {}: {}",
                file_path.display(),
                err
            );
        }
        crate::lsp::fresh_wait::wait_for_post_edit_diagnostics_unlocked(
            || manager.lock(),
            file_path,
            &expected_versions,
            &pre_snapshot,
            deadline,
        )
    }

    /// Collect custom server root_markers from user config for use in
    /// `is_config_file_path_with_custom` checks (#25).
    fn custom_lsp_root_markers(&self) -> Vec<String> {
        self.config()
            .lsp_servers
            .iter()
            .flat_map(|s| s.root_markers.iter().cloned())
            .collect()
    }

    fn notify_watched_config_files(&self, file_paths: &[PathBuf]) {
        let custom_markers = self.custom_lsp_root_markers();
        let config_paths: Vec<(PathBuf, FileChangeType)> = file_paths
            .iter()
            .filter(|path| is_config_file_path_with_custom(path, &custom_markers))
            .cloned()
            .map(|path| {
                let change_type = if path.exists() {
                    FileChangeType::CHANGED
                } else {
                    FileChangeType::DELETED
                };
                (path, change_type)
            })
            .collect();

        self.notify_watched_config_events(&config_paths);
    }

    fn multi_file_write_paths(params: &serde_json::Value) -> Option<Vec<PathBuf>> {
        let paths = params
            .get("multi_file_write_paths")
            .and_then(|value| value.as_array())?
            .iter()
            .filter_map(|value| value.as_str())
            .map(PathBuf::from)
            .collect::<Vec<_>>();

        (!paths.is_empty()).then_some(paths)
    }

    /// Parse config-file watched events from `multi_file_write_paths` when the
    /// array contains object entries `{ "path": "...", "type": "created|changed|deleted" }`.
    ///
    /// This handles the OBJECT variant of `multi_file_write_paths`. The STRING
    /// variant (bare path strings) is handled by `multi_file_write_paths()` and
    /// `notify_watched_config_files()`. Both variants read the same JSON key but
    /// with different per-entry schemas — they are NOT redundant.
    ///
    /// #18 note: in older code this function also existed alongside `multi_file_write_paths()`
    /// and was reachable via the `else if` branch when all entries were objects.
    /// Restoring both is correct.
    fn watched_file_events_from_params(
        params: &serde_json::Value,
        extra_markers: &[String],
    ) -> Option<Vec<(PathBuf, FileChangeType)>> {
        let events = params
            .get("multi_file_write_paths")
            .and_then(|value| value.as_array())?
            .iter()
            .filter_map(|entry| {
                // Only handle object entries — string entries go through multi_file_write_paths()
                let path = entry
                    .get("path")
                    .and_then(|value| value.as_str())
                    .map(PathBuf::from)?;

                if !is_config_file_path_with_custom(&path, extra_markers) {
                    return None;
                }

                let change_type = entry
                    .get("type")
                    .and_then(|value| value.as_str())
                    .and_then(Self::parse_file_change_type)
                    .unwrap_or_else(|| Self::change_type_from_current_state(&path));

                Some((path, change_type))
            })
            .collect::<Vec<_>>();

        (!events.is_empty()).then_some(events)
    }

    fn parse_file_change_type(value: &str) -> Option<FileChangeType> {
        match value {
            "created" | "CREATED" | "Created" => Some(FileChangeType::CREATED),
            "changed" | "CHANGED" | "Changed" => Some(FileChangeType::CHANGED),
            "deleted" | "DELETED" | "Deleted" => Some(FileChangeType::DELETED),
            _ => None,
        }
    }

    fn change_type_from_current_state(path: &Path) -> FileChangeType {
        if path.exists() {
            FileChangeType::CHANGED
        } else {
            FileChangeType::DELETED
        }
    }

    fn notify_watched_config_events(&self, config_paths: &[(PathBuf, FileChangeType)]) {
        if config_paths.is_empty() {
            return;
        }

        let config = self.config();
        let stamps = crate::lsp::manager::capture_watched_file_stamps(
            config_paths.iter().map(|(path, _)| path),
        );
        crate::lsp::pending_changes::send_or_queue_lsp_change(
            &self.lsp_manager,
            &self.lsp_pending_change_slot,
            Arc::clone(&config),
            |lsp| {
                if let Err(e) = lsp.notify_files_watched_changed_with_stamps(config_paths, &stamps)
                {
                    crate::slog_warn!("watched-file sync error: {}", e);
                }
            },
            |pending| pending.queue_watched_with_stamps(config_paths, &stamps),
        );
    }

    /// Publish final file membership, independently of delayed OS watcher echoes.
    /// Content-only edits continue through the existing document notifications.
    pub(crate) fn lsp_notify_mutation_file_events(&self, events: &[(PathBuf, FileChangeType)]) {
        if events.is_empty() {
            return;
        }
        let config = self.config();
        let markers = self.custom_lsp_root_markers();
        let stamps =
            crate::lsp::manager::capture_watched_file_stamps(events.iter().map(|(path, _)| path));
        crate::lsp::pending_changes::send_or_queue_lsp_change(
            &self.lsp_manager,
            &self.lsp_pending_change_slot,
            config,
            |lsp| {
                lsp.forward_watcher_file_events_with_stamps(events, &markers, &stamps);
            },
            |pending| pending.queue_mutation_watched(events, &stamps),
        );
    }

    pub fn lsp_notify_watched_config_file(&self, file_path: &Path, change_type: FileChangeType) {
        // An agent write to package.json or a lockfile can change which
        // TypeScript server a file gets, even with the file watcher off.
        crate::lsp::typescript_project::invalidate_typescript_selection([file_path]);
        let custom_markers = self.custom_lsp_root_markers();
        if !is_config_file_path_with_custom(file_path, &custom_markers) {
            return;
        }

        self.notify_watched_config_events(&[(file_path.to_path_buf(), change_type)]);
    }

    /// Post-write LSP hook for multi-file edits. When the patch includes
    /// config-file edits, notify active workspace servers via
    /// `workspace/didChangeWatchedFiles` before sending the per-document
    /// didOpen/didChange for the current file.
    pub fn lsp_post_multi_file_write(
        &self,
        file_path: &Path,
        content: &str,
        file_paths: &[PathBuf],
        params: &serde_json::Value,
    ) -> Option<crate::lsp::manager::PostEditWaitOutcome> {
        self.notify_watched_config_files(file_paths);
        self.add_pending_tier2_paths(file_paths.iter().cloned());

        let wants_diagnostics = params
            .get("diagnostics")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if !wants_diagnostics {
            self.lsp_notify_file_changed(file_path, content);
            return None;
        }

        let wait_ms = params
            .get("wait_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(3000)
            .min(10_000);

        Some(self.lsp_notify_and_collect_diagnostics(
            file_path,
            content,
            std::time::Duration::from_millis(wait_ms),
        ))
    }

    /// Post-write LSP hook: notify server and optionally collect diagnostics.
    ///
    /// This is the single call site for all command handlers after `write_format_validate`.
    /// Behavior:
    /// - When `diagnostics: true` is in `params`, notifies the server, waits
    ///   until matching diagnostics arrive or the timeout expires, and returns
    ///   `Some(outcome)` with the verified-fresh diagnostics + per-server
    ///   status.
    /// - When `diagnostics: false` (or absent), just notifies (fire-and-forget)
    ///   and returns `None`. Callers must NOT wrap this in `Some(...)`; the
    ///   `None` is what tells the response builder to omit the LSP fields
    ///   entirely (preserves the no-diagnostics-requested response shape).
    ///
    /// v0.17.3: default `wait_ms` raised from 1500 to 3000 because real-world
    /// tsserver re-analysis on monorepo files routinely takes 2-5s. Still
    /// capped at 10000ms.
    pub fn lsp_post_write(
        &self,
        file_path: &Path,
        content: &str,
        params: &serde_json::Value,
    ) -> Option<crate::lsp::manager::PostEditWaitOutcome> {
        let _phase = crate::logging::time_edit_phase(crate::logging::EditPhase::Lsp);
        let wants_diagnostics = params
            .get("diagnostics")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let custom_markers = self.custom_lsp_root_markers();
        if let Some(file_paths) = Self::multi_file_write_paths(params) {
            self.add_pending_tier2_paths(file_paths);
        } else {
            self.add_pending_tier2_paths([file_path.to_path_buf()]);
        }

        if !wants_diagnostics {
            if let Some(file_paths) = Self::multi_file_write_paths(params) {
                self.notify_watched_config_files(&file_paths);
            } else if let Some(config_events) =
                Self::watched_file_events_from_params(params, &custom_markers)
            {
                self.notify_watched_config_events(&config_events);
            }
            self.lsp_notify_file_changed(file_path, content);
            return None;
        }

        let wait_ms = params
            .get("wait_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(3000)
            .min(10_000); // Cap at 10 seconds to prevent hangs from adversarial input

        if let Some(file_paths) = Self::multi_file_write_paths(params) {
            return self.lsp_post_multi_file_write(file_path, content, &file_paths, params);
        }

        if let Some(config_events) = Self::watched_file_events_from_params(params, &custom_markers)
        {
            self.notify_watched_config_events(&config_events);
        }

        Some(self.lsp_notify_and_collect_diagnostics(
            file_path,
            content,
            std::time::Duration::from_millis(wait_ms),
        ))
    }

    fn resolved_path_restriction_root(&self, root: &Path) -> PathBuf {
        let mut memo = self.path_restriction_root_memo.lock();
        if let Some(cached) = memo.as_ref() {
            let root_exists = cached.configured_root.as_os_str() == root.as_os_str()
                && if crate::bounded_io::current().is_some() {
                    crate::bounded_io::metadata(&cached.resolved_root).is_ok()
                } else {
                    cached.resolved_root.exists()
                };
            if root_exists {
                return cached.resolved_root.clone();
            }
        }

        // A cache hit performs one `exists` stat instead of walking the root's
        // symlink chain. If the resolved root disappears, retry canonicalization
        // so deletion and recreation can choose its new identity. A retargeted
        // configured-root symlink whose previous target still exists is the
        // residual window until reconfigure or that target disappears.
        #[cfg(test)]
        self.path_restriction_root_canonicalizations
            .fetch_add(1, Ordering::SeqCst);
        let resolved_root = if crate::bounded_io::current().is_some() {
            match crate::bounded_io::canonicalize(root) {
                Ok(root) => root,
                // The request will report a filesystem timeout; do not cache a lexical
                // root identity from a timed-out probe for subsequent requests.
                Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {
                    return root.to_path_buf()
                }
                Err(_) => root.to_path_buf(),
            }
        } else {
            std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf())
        };
        *memo = Some(PathRestrictionRootMemo {
            configured_root: root.to_path_buf(),
            resolved_root: resolved_root.clone(),
        });
        resolved_root
    }

    fn path_restriction_context(
        &self,
        req_id: &str,
        path: &Path,
    ) -> Result<Option<PathRestrictionContext>, crate::protocol::Response> {
        let config = self.config();
        let force_restrict = self.request_force_restrict(req_id);
        if crate::access_policy::OutsideRoots::for_request(&config, force_restrict)
            == crate::access_policy::OutsideRoots::Ask
        {
            return Ok(None);
        }
        let root = match &config.project_root {
            Some(root) => root.clone(),
            None if force_restrict => {
                return Err(crate::protocol::Response::error(
                    req_id,
                    "path_outside_root",
                    "project root is required when path restriction is forced",
                ));
            }
            None => return Ok(None),
        };
        drop(config);

        let raw_root = root.clone();
        let resolved_root = self.resolved_path_restriction_root(&root);
        let path_for_resolution = if path.is_relative() {
            raw_root.join(path)
        } else {
            path.to_path_buf()
        };
        Ok(Some(PathRestrictionContext {
            raw_root,
            resolved_root,
            path_for_resolution,
        }))
    }

    /// Resolve a possibly-relative path against the configured project root.
    ///
    /// Safety arms that key backup/checkpoint state by path (`undo`,
    /// `undo_preview`, `edit_history`, `checkpoint`) must resolve relative paths
    /// against the request's bound project root BEFORE validation and keying.
    /// Otherwise a relative path is joined against the daemon's current working
    /// directory by `canonicalize_key`, which differs from the root the mutating
    /// tool resolved against — the per-(session, path) stack lookup then misses
    /// and the user gets a false `no_undo_history`.
    ///
    /// When no `project_root` is configured (direct CLI usage), relative paths
    /// fall back to the current working directory, matching `canonicalize_key`.
    pub fn resolve_relative_path(&self, path: &Path) -> PathBuf {
        // A GitHub address is left as spelled so the path validator that
        // follows recognises and refuses it; joined onto the root it would
        // look like an ordinary `<root>/issue:/N` file.
        if path.is_absolute()
            || path
                .to_str()
                .is_some_and(crate::github_address::is_github_resource_path)
        {
            return path.to_path_buf();
        }
        if let Some(root) = &self.config().project_root {
            return root.join(path);
        }
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }

    /// Validate that a file path falls within the configured project root.
    ///
    /// When `project_root` is configured (normal plugin usage), this resolves the
    /// path and checks it starts with the root. Returns the canonicalized path on
    /// success, or an error response on violation.
    ///
    /// When no `project_root` is configured (direct CLI usage), outside asks retain
    /// the unrestricted legacy behavior. The access-policy floor still applies.
    pub fn validate_path(
        &self,
        req_id: &str,
        path: &Path,
    ) -> Result<std::path::PathBuf, crate::protocol::Response> {
        // Mutating tools validate here, so a helper that is merely slow to be
        // scheduled must not refuse the user's change (see `MutationScope`).
        let _io = crate::bounded_io::enter_mutation();
        self.validate_path_with_artifact_session(req_id, path, None, None, true)
    }

    /// Validate a write location without following its final path component.
    ///
    /// Checkpoint creation and restore use this mode because the final component
    /// is the object being preserved or replaced. Following a symlink there would
    /// authorize its target and change the stored snapshot key. Every ancestor is
    /// still resolved so a symlinked parent cannot escape the project root.
    pub fn validate_write_location(
        &self,
        req_id: &str,
        path: &Path,
    ) -> Result<std::path::PathBuf, crate::protocol::Response> {
        self.authorize_write_location(req_id, path)
            .map(|(location, _)| location)
    }

    /// [`AppContext::validate_write_location`], also returning the identity
    /// the access policy judged: the canonical parent plus the final
    /// component as spelled. A caller that holds the parent directory open
    /// can check that the directory it holds is the one that was judged.
    pub(crate) fn authorize_write_location(
        &self,
        req_id: &str,
        path: &Path,
    ) -> Result<(PathBuf, PathBuf), crate::protocol::Response> {
        // Only mutations validate write locations (see `MutationScope`).
        let _io = crate::bounded_io::enter_mutation();
        reject_github_address(req_id, path)?;
        self.check_unrestored_path(
            req_id,
            &crate::path_identity::normalize_path(&self.resolve_relative_path(path)),
            None,
            true,
        )?;
        let Some(PathRestrictionContext {
            raw_root,
            resolved_root,
            path_for_resolution,
        }) = self.path_restriction_context(req_id, path)?
        else {
            // Root restriction is off, so the caller's spelling is kept, but a
            // relative spelling is rooted at the project root (or the working
            // directory when none is configured): that location
            // is what gets checked, and the handler must touch the same one
            // rather than resolve the spelling against the daemon's cwd.
            let rooted = self.resolve_relative_path(path);
            let identity = self.check_path_access(req_id, path, &rooted, None, None, true, true)?;
            return Ok((rooted, identity));
        };
        let normalized = normalize_path(&path_for_resolution);
        let Some(file_name) = normalized.file_name() else {
            let location = self.validate_path(req_id, path)?;
            let identity = crate::access_policy::policy_path(&location);
            return Ok((location, identity));
        };
        let parent = normalized.parent().unwrap_or_else(|| Path::new(""));
        let id = req_id.to_string();
        let original = path.to_path_buf();
        let resolved_parent = crate::bounded_io::run(parent, None, move |parent| {
            Ok(match std::fs::canonicalize(&parent) {
                Ok(resolved) => Ok(resolved),
                Err(_) => {
                    reject_escaping_symlink(&id, &original, &parent, &resolved_root, &raw_root)
                        .map(|()| resolve_with_existing_ancestors(&parent))
                }
            })
        })
        .map_err(|error| {
            crate::protocol::Response::error(
                req_id,
                crate::bounded_io::timeout_code(&error),
                error.to_string(),
            )
        })??;
        let resolved = normalize_path(&resolved_parent.join(file_name));

        let identity = self.check_path_access(req_id, path, &resolved, None, None, true, true)?;
        Ok((resolved, identity))
    }

    /// Validate a read path. Saved tool results, registered bash output and a task's
    /// recorded scratch directory may live outside the project root; their owning
    /// session may read them without an additional permission request. Mutating
    /// tools deliberately use
    /// [`AppContext::validate_path`] or [`AppContext::validate_write_location`]
    /// and never receive this exception.
    pub fn validate_read_path(
        &self,
        req_id: &str,
        session_id: &str,
        path: &Path,
    ) -> Result<std::path::PathBuf, crate::protocol::Response> {
        self.validate_read_path_with_approval(req_id, session_id, path, true)
    }

    /// Apply the normal session read policy without entering an approval route.
    /// Keep the caller's policy context intact, including plugin read exemptions,
    /// but refuse a path that would require an outside-access ask.
    pub(crate) fn validate_read_path_without_approval(
        &self,
        req_id: &str,
        session_id: &str,
        path: &Path,
    ) -> Result<std::path::PathBuf, crate::protocol::Response> {
        self.validate_read_path_with_approval(req_id, session_id, path, false)
    }

    fn validate_read_path_with_approval(
        &self,
        req_id: &str,
        session_id: &str,
        path: &Path,
        allow_approval: bool,
    ) -> Result<std::path::PathBuf, crate::protocol::Response> {
        self.output_files
            .restore(self, session_id, &self.resolve_relative_path(path));
        let result = self.validate_path_with_artifact_session(
            req_id,
            path,
            Some(session_id),
            None,
            allow_approval,
        );
        if crate::context::confinement::for_request(req_id) {
            // Canonical targets and symlink destinations are private outside the
            // fence. Always report the caller's spelling, including on failure.
            result.map_err(|_| crate::context::confinement::refusal(req_id, path))
        } else {
            result
        }
    }

    pub(crate) fn validate_search_path(
        &self,
        req_id: &str,
        session_id: &str,
        path: &Path,
        search_tool: WorkerSearchTool,
    ) -> Result<std::path::PathBuf, crate::protocol::Response> {
        self.validate_path_with_artifact_session(
            req_id,
            path,
            Some(session_id),
            Some(search_tool),
            true,
        )
    }

    /// Refuse `location` unless it lies under the resolved project root,
    /// compared lexically component by component without following any
    /// symlink. For locations that are not looked up on today's disk, such as
    /// a file inside an older git commit: the commit may hold a real
    /// directory where a symlink into the root stands today, so a check that
    /// follows today's symlinks would admit it. `location` must begin with a
    /// fully resolved directory (as `fs::canonicalize` returns it), such as
    /// the repository's resolved root, so it compares with the resolved
    /// project root. `path` is the caller's spelling, used in the refusal. A
    /// no-op when reads are not restricted to the project root for this
    /// request.
    pub(crate) fn check_location_lexically_in_root(
        &self,
        req_id: &str,
        path: &Path,
        location: &Path,
    ) -> Result<(), crate::protocol::Response> {
        let Some(PathRestrictionContext { resolved_root, .. }) =
            self.path_restriction_context(req_id, path)?
        else {
            return Ok(());
        };
        if normalize_path(location).starts_with(&resolved_root) {
            return Ok(());
        }
        if crate::context::confinement::for_request(req_id) {
            // As in `validate_read_path`: confined requests never learn the
            // resolved root, only their own spelling.
            return Err(crate::context::confinement::refusal(req_id, path));
        }
        Err(path_error_response(req_id, path, &resolved_root))
    }

    fn validate_path_with_artifact_session(
        &self,
        req_id: &str,
        path: &Path,
        artifact_session_id: Option<&str>,
        search_tool: Option<WorkerSearchTool>,
        allow_approval: bool,
    ) -> Result<std::path::PathBuf, crate::protocol::Response> {
        let _io = crate::bounded_io::enter_if_absent(crate::bounded_io::IO_BUDGET);
        reject_github_address(req_id, path)?;
        self.check_unrestored_path(
            req_id,
            &crate::path_identity::normalize_path(&self.resolve_relative_path(path)),
            artifact_session_id,
            false,
        )?;
        let Some(PathRestrictionContext {
            raw_root,
            resolved_root,
            path_for_resolution,
        }) = self.path_restriction_context(req_id, path)?
        else {
            // Root restriction is off, so the caller's spelling is kept, but a
            // relative spelling is rooted at the project root (or the working
            // directory when none is configured): that location
            // is what gets checked, and the handler must read the same one
            // rather than resolve the spelling against the daemon's cwd.
            let rooted = self.resolve_relative_path(path);
            // Scratch readers use the canonical spelling checked by policy so
            // aliases such as /tmp on macOS cannot evade the opened-handle fence.
            if let Some(session) = artifact_session_id {
                if self
                    .bash_background
                    .is_session_owned_scratch_path(session, &rooted)
                {
                    let resolved = crate::bounded_io::canonicalize(&rooted).map_err(|error| {
                        crate::protocol::Response::error(req_id, "read_blocked", error.to_string())
                    })?;
                    self.check_path_access(
                        req_id,
                        path,
                        &resolved,
                        artifact_session_id,
                        search_tool,
                        false,
                        allow_approval,
                    )?;
                    return Ok(resolved);
                }
            }
            self.check_path_access(
                req_id,
                path,
                &rooted,
                artifact_session_id,
                search_tool,
                false,
                allow_approval,
            )?;
            return Ok(rooted);
        };

        // Resolve the path (follow symlinks, normalize ..). If canonicalization
        // fails (e.g. path does not exist or traverses a broken symlink), inspect
        // every existing component with lstat before falling back lexically so a
        // broken in-root symlink cannot be used to write outside project_root.
        let resolve = |req_id: &str, path: &Path, resolved_root: &Path, raw_root: &Path| {
            match std::fs::canonicalize(path) {
                Ok(resolved) => Ok(resolved),
                Err(_) => {
                    let normalized = normalize_path(path);
                    reject_escaping_symlink(req_id, path, &normalized, resolved_root, raw_root)?;
                    Ok(resolve_with_existing_ancestors(&normalized))
                }
            }
        };
        let resolved = if crate::bounded_io::current().is_some() {
            // Even authorization's symlink/ancestor probes can block on a
            // remote volume. Keep the work owned and fail closed on timeout;
            // never fall back to a lexical authorization after a timed-out stat.
            let id = req_id.to_string();
            let owned_resolved_root = resolved_root.clone();
            let owned_raw_root = raw_root.clone();
            crate::bounded_io::run(&path_for_resolution, None, move |path| {
                Ok(resolve(&id, &path, &owned_resolved_root, &owned_raw_root))
            })
            .map_err(|error| {
                crate::protocol::Response::error(
                    req_id,
                    crate::bounded_io::timeout_code(&error),
                    error.to_string(),
                )
            })??
        } else {
            resolve(req_id, &path_for_resolution, &resolved_root, &raw_root)?
        };

        self.check_path_access(
            req_id,
            path,
            &resolved,
            artifact_session_id,
            search_tool,
            false,
            allow_approval,
        )?;
        Ok(resolved)
    }

    /// Read saved launch inputs without filesystem probes and combine them with
    /// confirmed-absence records. Neither source can authorize path access.
    pub(crate) fn unrestored_launch_grants(
        &self,
        req_id: &str,
    ) -> Result<Vec<launch_grants::UnrestoredGrant>, crate::protocol::Response> {
        if let Some(entry) = read_policy_cache::current(self as *const Self as usize, req_id) {
            if let Some(grants) = entry.unrestored() {
                return Ok(grants);
            }
            let grants = self.load_unrestored_launch_grants(req_id)?;
            entry.set_unrestored(grants.clone());
            return Ok(grants);
        }
        self.load_unrestored_launch_grants(req_id)
    }

    fn load_unrestored_launch_grants(
        &self,
        req_id: &str,
    ) -> Result<Vec<launch_grants::UnrestoredGrant>, crate::protocol::Response> {
        let crate::access_policy::AskRoute::Consent {
            launch: Some(key), ..
        } = crate::access_policy::current_ask_route()
        else {
            return Ok(Vec::new());
        };
        let unavailable = |message: String| {
            crate::protocol::Response::error_with_data(
                req_id,
                "grants_unavailable",
                message,
                serde_json::json!({"detail":{"retryable":true,"phase":"launch_grant_restore","path":key.project_root}}),
            )
        };
        let db = self
            .db()
            .ok_or_else(|| unavailable("scope persistence unavailable; retry".into()))?;
        let conn = db
            .lock()
            .map_err(|_| unavailable("grant database lock poisoned; retry".into()))?;
        let pending = crate::db::access_grants::unrestored_launch(&conn, &key)
            .map_err(|error| unavailable(format!("launch grants unavailable: {error}; retry")))?;
        Ok(self.launch_grant_state.unavailable(&key, pending))
    }

    pub(crate) fn unrestored_grant_refusal(
        req_id: &str,
        grant: &launch_grants::UnrestoredGrant,
    ) -> crate::protocol::Response {
        let path = &grant.path;
        let status = if grant.retryable {
            "is not restored; retry after the path recovers"
        } else {
            "no longer exists; this saved grant cannot be restored"
        };
        crate::protocol::Response::error_with_data(
            req_id,
            "grants_unavailable",
            format!("launch grant for {} {status}", path.display()),
            serde_json::json!({"detail":{"retryable":grant.retryable,"path":path,"phase":"launch_grant_restore"}}),
        )
    }

    fn check_unrestored_path(
        &self,
        req_id: &str,
        path: &Path,
        artifact_session: Option<&str>,
        preserve_final: bool,
    ) -> Result<(), crate::protocol::Response> {
        use crate::access_policy::{AccessDecision, AccessDenial, AccessOperation};
        let operation = if artifact_session.is_some() {
            AccessOperation::Read
        } else {
            AccessOperation::Write
        };
        let pending = self.unrestored_launch_grants(req_id)?;
        let Some(grant) = pending.iter().find(|grant| grant.covers(operation, path)) else {
            return Ok(());
        };
        let owned_artifact = artifact_session.is_some_and(|session| {
            self.output_files.owner(path).as_deref() == Some(session)
                || self
                    .bash_background
                    .is_session_owned_artifact_path(session, path)
                || self
                    .bash_background
                    .is_session_owned_scratch_path(session, path)
                || self.bash_background.is_readable_task_path(session, path)
        });
        let policy = self.access_policy(req_id)?;
        let decision = if preserve_final {
            policy.decide_tree_write(path)
        } else {
            policy.decide(operation, path, owned_artifact)
        };
        // This check grants nothing. Canonical resolution and the usual policy
        // still follow, including floors, symlink checks and owned-artifact rules.
        if !matches!(
            decision,
            AccessDecision::Allow | AccessDecision::Deny(AccessDenial::Floor)
        ) {
            return Err(Self::unrestored_grant_refusal(req_id, grant));
        }
        Ok(())
    }

    /// Use the effective configuration snapshot selected for the current tool route.
    /// Untrusted subc calls may also force outside roots: deny for this request,
    /// even when the user's configuration asks (see [`crate::access_policy::OutsideRoots`]).
    pub fn access_policy(
        &self,
        req_id: &str,
    ) -> Result<crate::access_policy::AccessPolicy, crate::protocol::Response> {
        if read_policy_cache::current(self as *const Self as usize, req_id).is_some() {
            return self
                .shared_access_policy(req_id)
                .map(|policy| (*policy).clone());
        }
        self.build_access_policy(req_id)
    }

    /// Keep authority stable within one read-list request, never across calls,
    /// context changes, scope epochs or approval-route changes.
    pub(crate) fn with_read_policy_snapshot<R>(&self, req_id: &str, run: impl FnOnce() -> R) -> R {
        read_policy_cache::with(self as *const Self as usize, req_id, run)
    }

    fn shared_access_policy(
        &self,
        req_id: &str,
    ) -> Result<Arc<crate::access_policy::AccessPolicy>, crate::protocol::Response> {
        if let Some(entry) = read_policy_cache::current(self as *const Self as usize, req_id) {
            if let Some(policy) = entry.get() {
                return Ok(policy);
            }
            let policy = Arc::new(self.build_access_policy(req_id)?);
            entry.set(policy.clone());
            return Ok(policy);
        }
        self.build_access_policy(req_id).map(Arc::new)
    }

    fn build_access_policy(
        &self,
        req_id: &str,
    ) -> Result<crate::access_policy::AccessPolicy, crate::protocol::Response> {
        #[cfg(test)]
        policy_load_probe::policy(req_id);
        use crate::access_policy::{AccessDecision, AccessDenial, AccessPolicy, OutsideRoots};
        let config = self.config();
        let standalone = matches!(
            crate::access_policy::current_ask_route(),
            crate::access_policy::AskRoute::Plugin { .. }
        );
        let roots = config
            .project_root
            .as_ref()
            .map(|root| vec![self.resolved_path_restriction_root(root)])
            .unwrap_or_default();
        let force_restrict = self.request_force_restrict(req_id);
        // The standalone exemptions (main checkout, temp scratch) only apply
        // while outside paths ask; outside roots: deny refuses them.
        let outside_ask = OutsideRoots::for_request(&config, force_restrict) == OutsideRoots::Ask;
        let mut main_roots = Vec::new();
        if standalone && outside_ask {
            // Frozen when the cache role was set, after the linkage between
            // this worktree and its main checkout was verified. It is never
            // resolved again here, so a later symlink or junction swap of the
            // main checkout's path cannot move it.
            if let Some(main) = self.linked_main_root.lock().clone() {
                if !roots
                    .iter()
                    .any(|root| crate::path_identity::normalize_path(root) == main)
                {
                    main_roots.push(main);
                }
            }
        }
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
            .unwrap_or_default();
        let storage = self.storage_dir();
        let (grants, mode, worker) = if let Some(scope) = crate::access_policy::current_scope() {
            #[cfg(test)]
            policy_load_probe::grants(req_id);
            match self.db() {
                Some(db) => {
                    let conn = db.lock().map_err(|_| {
                        crate::protocol::Response::error(
                            req_id,
                            "grants_unavailable",
                            "grant database lock poisoned",
                        )
                    })?;
                    let grants =
                        crate::db::access_grants::list(&conn, &scope).map_err(|error| {
                            crate::protocol::Response::error(
                                req_id,
                                "grants_unavailable",
                                error.to_string(),
                            )
                        })?;
                    let mode = crate::db::access_grants::mode(&conn, &scope).map_err(|error| {
                        crate::protocol::Response::error(
                            req_id,
                            "grants_unavailable",
                            error.to_string(),
                        )
                    })?;
                    let worker =
                        crate::db::access_grants::is_worker(&conn, &scope).map_err(|error| {
                            crate::protocol::Response::error(
                                req_id,
                                "grants_unavailable",
                                error.to_string(),
                            )
                        })?;
                    (grants, mode, worker)
                }
                None => (Vec::new(), Default::default(), false),
            }
        } else {
            (Vec::new(), Default::default(), false)
        };
        let elicitation = matches!(
            crate::access_policy::current_ask_route(),
            crate::access_policy::AskRoute::Elicitation { .. }
        );
        // A known plugin host whose thread lost its route keeps the temp
        // guards too. Without them an in-root path under a system temp
        // directory would skip the database, hard-link and foreign-scratch
        // refusals that the admitted route applies.
        let guarded = standalone || self.missing_plugin_route();
        let temp_override = crate::access_policy::temp::roots_override();
        let scope_grants_denial = crate::access_policy::scope_grants_denial();
        let build = move || {
            let _grants = crate::access_policy::scope_grants_guard(scope_grants_denial);
            let mut policy = AccessPolicy::from_config(&config, &storage, roots, &home);
            policy.standalone_main_roots = main_roots;
            if guarded {
                policy.standalone_temp_guard_roots =
                    crate::access_policy::temp::guard_roots(temp_override.as_deref());
                policy.standalone_managed_temp_roots = crate::access_policy::temp::managed_roots();
            }
            policy.install_scope_grants(grants, worker);
            policy.mode = mode;
            if standalone && outside_ask {
                policy.standalone_temp_roots =
                    crate::access_policy::temp::allowed_roots(temp_override.as_deref());
            }
            if force_restrict && !elicitation {
                policy.outside = AccessDecision::Deny(AccessDenial::OutsideRoots);
            }
            policy
        };
        if crate::bounded_io::current().is_some() {
            crate::bounded_io::run(Path::new("."), None, move |_| Ok(build())).map_err(|error| {
                crate::protocol::Response::error(
                    req_id,
                    crate::bounded_io::timeout_code(&error),
                    error.to_string(),
                )
            })
        } else {
            Ok(build())
        }
    }

    fn checkout_root_for_message(&self) -> Option<PathBuf> {
        let root = self
            .config()
            .project_root
            .clone()
            .or_else(|| std::env::current_dir().ok())?;
        Some(root)
    }

    fn current_call_is_worker(&self) -> bool {
        let Some(scope) = crate::access_policy::current_scope() else {
            return false;
        };
        let Some(db) = self.db() else {
            return false;
        };
        let Ok(conn) = db.lock() else {
            return false;
        };
        crate::db::access_grants::is_worker(&conn, &scope).unwrap_or(false)
    }

    fn worker_root_search_refusal(
        &self,
        req_id: &str,
        original: &Path,
        search_tool: Option<WorkerSearchTool>,
    ) -> Option<crate::protocol::Response> {
        search_tool?;
        if !relative_components_under_root(&normalize_path(original))
            .is_some_and(|relative| relative.as_os_str().is_empty())
            || !self.current_call_is_worker()
            || !matches!(
                crate::access_policy::current_ask_route(),
                crate::access_policy::AskRoute::Consent { .. }
            )
        {
            return None;
        }
        let checkout = self.checkout_root_for_message()?;
        Some(crate::protocol::Response::error(
            req_id,
            "worker_root_search",
            format!("use your checkout instead: {}", checkout.display()),
        ))
    }

    fn checkout_path_suggestion(&self, original: &Path) -> Option<PathBuf> {
        if !original.has_root() || !self.current_call_is_worker() {
            return None;
        }
        let checkout = self.checkout_root_for_message()?;
        if crate::bounded_io::current().is_some() {
            crate::bounded_io::run(original, None, move |path| {
                Ok(checkout_counterpart(&path, &checkout))
            })
            .ok()
            .flatten()
        } else {
            checkout_counterpart(original, &checkout)
        }
    }

    /// Authorize the repository location of a historical read without resolving
    /// today's symlinks. The current target's permission cannot authorize the
    /// different content stored under the original name in a commit.
    pub(crate) fn check_historical_read_location(
        &self,
        req_id: &str,
        location: &Path,
    ) -> Result<(), crate::protocol::Response> {
        use crate::access_policy::{AccessDecision, AccessOperation};
        let identity = crate::path_identity::normalize_path(location);
        let config = self.config();
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
            .unwrap_or_default();
        // Configured denies protect their written location too, even if that
        // location has since become a symlink to an allowed file or directory.
        let denied = config.sandbox.read_deny.iter().any(|path| {
            let path = crate::access_policy::expand_home(path, &home);
            let absolute = if path.is_absolute() {
                path
            } else {
                std::env::current_dir().unwrap_or_default().join(path)
            };
            identity.starts_with(crate::path_identity::normalize_path(&absolute))
        });
        let decision = self
            .access_policy(req_id)?
            .decide(AccessOperation::Read, &identity, false);
        if denied || matches!(decision, AccessDecision::Deny(_)) {
            return Err(crate::protocol::Response::error(
                req_id,
                "denied_floor",
                format!(
                    "historical path '{}' is denied by the access policy",
                    location.display()
                ),
            ));
        }
        if decision == AccessDecision::Ask {
            self.answer_ask(req_id, location, &identity, AccessOperation::Read)?;
        }
        Ok(())
    }

    pub(crate) fn mark_standalone_plugin_host(&self) {
        self.standalone_plugin_host.store(true, Ordering::Release);
    }

    pub(crate) fn missing_plugin_route(&self) -> bool {
        matches!(
            crate::access_policy::current_ask_route(),
            crate::access_policy::AskRoute::Lowered
        ) && (self.standalone_plugin_host.load(Ordering::Acquire)
            || matches!(
                self.config().harness,
                Some(crate::harness::Harness::Opencode | crate::harness::Harness::Pi)
            ))
    }

    pub(crate) fn standalone_approval_scope(
        &self,
        session: &str,
    ) -> crate::access_policy::plugin::Scope {
        crate::access_policy::plugin::Scope::new(&self.standalone_approval_route, session)
    }

    /// Resolve an `ask` decision for the call on this thread. A lowered route
    /// treats the ask as allow. Plugins echo exact engine ask IDs; consent
    /// (`allow_once`) and reverse-elicitation routes allow only what was
    /// approved for this dispatch; anything else refuses with the internal ask
    /// code, carrying the capability to request. No UI means a path denial,
    /// not an implicit approval.
    fn answer_ask(
        &self,
        req_id: &str,
        original: &Path,
        identity: &Path,
        operation: crate::access_policy::AccessOperation,
    ) -> Result<(), crate::protocol::Response> {
        #[cfg(test)]
        confinement::note_ask();
        use crate::access_policy::{AskRoute, GrantKind};
        let route = crate::access_policy::current_ask_route();
        match &route {
            AskRoute::Lowered => return Ok(()),
            AskRoute::Unavailable => {
                return Err(crate::protocol::Response::error_with_data(
                    req_id, crate::access_policy::PERMISSION_ASK_UNAVAILABLE,
                    format!("{} access to '{}' needs permission, and a codemode program can't ask for it",
                        match operation { crate::access_policy::AccessOperation::Read => "read", crate::access_policy::AccessOperation::Write => "write" }, original.display()),
                    serde_json::json!({"permission":operation,"resource":identity}),
                ));
            }
            AskRoute::Deny => {
                let root = self.config().project_root.clone().unwrap_or_default();
                return Err(path_error_response(req_id, original, &root));
            }
            _ => {}
        }
        // A directory is asked for as a subtree so one answer covers what the
        // tool reads beneath it. The probe stays on the bounded helper when
        // one is active, like every other path probe on the request path.
        let probe = identity.to_path_buf();
        let is_dir = if crate::bounded_io::current().is_some() {
            crate::bounded_io::run(&probe, None, |path| match std::fs::metadata(path) {
                Ok(metadata) => Ok(metadata.is_dir()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
                Err(error) => Err(error),
            })
            .map_err(|error| {
                crate::protocol::Response::error(
                    req_id,
                    crate::bounded_io::timeout_code(&error),
                    error.to_string(),
                )
            })?
        } else {
            probe.is_dir()
        };
        let kind = if is_dir {
            GrantKind::Subtree
        } else {
            GrantKind::File
        };
        let grant = crate::access_policy::AccessGrant {
            path: identity.to_path_buf(),
            kind,
            operation,
        };
        if let AskRoute::Consent { once, .. } | AskRoute::Elicitation { once } = &route {
            if once
                .iter()
                .any(|approved| approved.covers_kind(operation, identity, kind))
            {
                return Ok(());
            }
        }
        // A worker naming the head's path for a file its own checkout has is
        // pointed at its checkout instead of being asked about the head's.
        if let Some(suggestion) = self.checkout_path_suggestion(original) {
            return Err(crate::protocol::Response::error(
                req_id,
                "path_not_found_did_you_mean",
                format!(
                    "path not found: {}; did you mean {}?",
                    original.display(),
                    crate::path_identity::normalize_path(&suggestion).display()
                ),
            ));
        }

        if let AskRoute::Plugin { scope, once } = route {
            // A file approval must not become a tree approval if the target
            // changed into a directory while the host prompt was open.
            if once
                .iter()
                .any(|approved| approved.covers_kind(operation, identity, kind))
            {
                return Ok(());
            }
            let id = crate::access_policy::plugin::issue(&scope, &grant);
            // OpenCode's `*` includes every descendant. Use the same folder
            // eligibility as consent cards so saving a file approval cannot
            // silently approve home, a volume root, or a system tree.
            let saved_scope = crate::subc::directory_scope(&grant);
            let pattern = match saved_scope.kind {
                GrantKind::File => saved_scope.path,
                GrantKind::Subtree => saved_scope.path.join("*"),
            };
            let mut ask = serde_json::json!({
                "kind": "external_directory", "patterns": [pattern], "always": [pattern],
                "grant_id": id, "path": identity, "operation": operation,
                "parentDir": if is_dir { identity } else { identity.parent().unwrap_or(identity) },
            });
            // Expiry and route replacement invalidate the old approval, not
            // the request. The host can ask again for the new route's token.
            if let Some(reason) = scope.retry_refusal_reason(&grant, !once.is_empty()) {
                scope.log_retry_refusal(operation, identity, reason);
                if matches!(reason, "approval_expired" | "approval_route_replaced") {
                    ask["reason"] = serde_json::json!(reason);
                }
            }
            return Err(crate::protocol::Response::error_with_data(
                req_id,
                crate::protocol::ERROR_PERMISSION_REQUIRED,
                format!(
                    "path '{}' needs outside-access permission",
                    original.display()
                ),
                serde_json::json!({"asks": [ask]}),
            ));
        }
        Err(crate::protocol::Response::error_with_data(
            req_id,
            crate::access_policy::ACCESS_ASK_REQUIRED,
            format!(
                "path '{}' is outside this worker's worktree and needs permission",
                original.display()
            ),
            serde_json::json!({
                "ask": grant,
            }),
        ))
    }

    /// Authorize `resolved` (the location for `original`) and return the
    /// identity the policy judged: the canonical path, or for a write
    /// location its canonical parent plus the final component as spelled.
    fn check_path_access(
        &self,
        req_id: &str,
        original: &Path,
        resolved: &Path,
        artifact_session: Option<&str>,
        search_tool: Option<WorkerSearchTool>,
        preserve_final: bool,
        allow_approval: bool,
    ) -> Result<PathBuf, crate::protocol::Response> {
        use crate::access_policy::{policy_path, AccessDecision, AccessDenial, AccessOperation};
        if let Some(response) = self.worker_root_search_refusal(req_id, original, search_tool) {
            return Err(response);
        }
        // A relative path names a file under the project root, which is where
        // the tools resolve it. Judge that location, not one under the
        // daemon's working directory: when root restriction is off the path
        // arrives here exactly as the caller spelled it.
        let rooted;
        let resolved = if resolved.is_relative() {
            rooted = self.resolve_relative_path(resolved);
            rooted.as_path()
        } else {
            resolved
        };
        let operation = if artifact_session.is_some() {
            AccessOperation::Read
        } else {
            AccessOperation::Write
        };
        #[cfg(test)]
        if operation == AccessOperation::Read {
            policy_load_probe::read_path(req_id);
        }
        let identify = move |resolved: PathBuf| {
            if preserve_final {
                // Resolve the parent before removing dot components: link/.. can
                // name the target's parent, not the link's containing directory.
                match (resolved.parent(), resolved.file_name()) {
                    (Some(parent), Some(name)) => policy_path(parent).join(name),
                    _ => policy_path(&resolved),
                }
            } else {
                policy_path(&resolved)
            }
        };
        let identity = if crate::bounded_io::current().is_some() {
            crate::bounded_io::run(resolved, None, move |path| Ok(identify(path))).map_err(
                |error| {
                    crate::protocol::Response::error(
                        req_id,
                        crate::bounded_io::timeout_code(&error),
                        error.to_string(),
                    )
                },
            )?
        } else {
            identify(resolved.to_path_buf())
        };
        let owned_tool_output = artifact_session
            .is_some_and(|session| self.output_files.owner(&identity).as_deref() == Some(session));
        if crate::context::confinement::for_request(req_id)
            && self.request_force_restrict(req_id)
            && !owned_tool_output
        {
            // `identity` went through `policy_path`, which drops the Windows
            // verbatim prefix (`\\?\C:\`) that `canonicalize` puts on the
            // root. Compare both in that same form, or nothing is in the root.
            let root = self.config().project_root.as_ref().map(|root| {
                crate::path_identity::normalize_path(&self.resolved_path_restriction_root(root))
            });
            if !root.is_some_and(|root| identity.starts_with(root))
                || !crate::bounded_io::metadata(&identity).is_ok()
            {
                return Err(crate::context::confinement::refusal(req_id, original));
            }
            // Only an exact, session-owned result file bypasses containment.
            // Credential checks and user-configured denials still apply below.
        }
        let owned_artifact = artifact_session.is_some_and(|session| {
            owned_tool_output
                || self
                    .bash_background
                    .is_session_owned_artifact_path(session, &identity)
                || self
                    .bash_background
                    .is_session_owned_scratch_path(session, &identity)
                || self
                    .bash_background
                    .is_readable_task_path(session, &identity)
        });
        if self.output_files.is_result_path(self, &identity)
            && (operation != AccessOperation::Read || !owned_tool_output)
        {
            return Err(crate::protocol::Response::error(
                req_id,
                "artifact_refused",
                "tool result belongs to another session or is read-only",
            ));
        }
        self.check_unrestored_path(req_id, &identity, artifact_session, preserve_final)?;
        let mut policy = self.shared_access_policy(req_id)?;
        // A lost thread-local route must not turn a known plugin host into the
        // legacy CLI allow path, even if that root's config has no harness yet.
        // It is reported after the policy decision so a path the policy denies
        // anyway (outside a restricted root, under a floor) keeps that answer.
        let route_missing = self.missing_plugin_route()
            && !policy.roots.iter().any(|root| identity.starts_with(root));
        let decide = |policy: &crate::access_policy::AccessPolicy| {
            if preserve_final {
                policy.decide_tree_write(&identity)
            } else if operation == AccessOperation::Read {
                policy.decide_resolved(operation, &identity, owned_artifact)
            } else {
                policy.decide(operation, &identity, owned_artifact)
            }
        };
        let mut decision = decide(&policy);
        if let crate::access_policy::AskRoute::Plugin { scope, .. } =
            crate::access_policy::current_ask_route()
        {
            if !matches!(
                decision,
                AccessDecision::Allow | AccessDecision::Deny(AccessDenial::Floor)
            ) {
                let registry = self.bash_background.clone();
                let session = scope.session().to_owned();
                // Ownership is judged on the entry the tool names. A read opens
                // the fully resolved target, so `identity` is that entry. A write
                // is judged on the requested spelling with its parent resolved
                // and its final component kept as spelled, the same identity the
                // write-location check uses. An outside entry that is a symlink
                // into the session's scratch is therefore a different entry and
                // never owned: deleting, replacing or writing through it still
                // asks, because those act on (or through) the outside link.
                let spelled = (artifact_session.is_none() && !preserve_final)
                    .then(|| self.resolve_relative_path(original));
                let location_identity = identity.clone();
                let owned_scratch = crate::bounded_io::run(&identity, None, move |_| {
                    let location = match spelled {
                        Some(spelled) => match (spelled.parent(), spelled.file_name()) {
                            (Some(parent), Some(name)) => policy_path(parent).join(name),
                            _ => policy_path(&spelled),
                        },
                        None => location_identity,
                    };
                    Ok(session_owned_scratch_location(
                        &registry, &session, &location,
                    ))
                })
                .map_err(|error| {
                    crate::protocol::Response::error(
                        req_id,
                        crate::bounded_io::timeout_code(&error),
                        error.to_string(),
                    )
                })?;
                if owned_scratch {
                    Arc::make_mut(&mut policy).standalone_owned_scratch = true;
                    decision = decide(&policy);
                }
            }
        }
        if route_missing && !matches!(decision, AccessDecision::Deny(_)) {
            return Err(crate::protocol::Response::error(
                req_id,
                "access_route_missing",
                "plugin filesystem access requires its admitted permission route",
            ));
        }
        if let AccessDecision::Deny(denial) = decision {
            if let crate::access_policy::AskRoute::Plugin { scope, .. } =
                crate::access_policy::current_ask_route()
            {
                let reason = match denial {
                    AccessDenial::Floor => "denied_floor",
                    AccessDenial::OutsideRoots => "outside_roots_denied",
                    AccessDenial::ReadOnly => "access_read_only",
                    AccessDenial::UnsafeTemp => "unsafe_temp_path",
                    AccessDenial::ScopeGrants(_) => "scope_grants_denied",
                };
                scope.log_retry_refusal(operation, &identity, reason);
            }
        }
        // Preserve the existing path_outside_root response when root restriction
        // already denied this path. The protected path is still rejected; changing
        // the error spelling must never turn a floor denial into a consent request.
        if decision == AccessDecision::Deny(AccessDenial::Floor)
            && self.request_outside_roots(req_id) == crate::access_policy::OutsideRoots::Deny
            && !policy.roots.iter().any(|root| identity.starts_with(root))
            && !(operation == AccessOperation::Read && owned_artifact)
        {
            let root = self
                .config()
                .project_root
                .as_ref()
                .map(|root| self.resolved_path_restriction_root(root));
            if let Some(root) = root {
                return Err(path_error_response(req_id, original, &root));
            }
        }
        match decision {
            AccessDecision::Allow => {
                crate::bounded_io::probe_access(req_id, &identity).map(|()| identity)
            }
            AccessDecision::Ask => {
                if !allow_approval {
                    let root = self.config().project_root.clone().unwrap_or_default();
                    return Err(path_error_response(req_id, original, &root));
                }
                let operation = if preserve_final {
                    AccessOperation::Write
                } else {
                    operation
                };
                // A read of the session's own scratch that no longer exists
                // (cleanup removes it when the command exits) has nothing to
                // grant. Answer not-found rather than raising a permission ask.
                if operation == AccessOperation::Read {
                    if let Some(session) = artifact_session {
                        if self
                            .bash_background
                            .is_session_missing_scratch_path(session, &identity)
                        {
                            return Err(crate::protocol::Response::error(
                                req_id,
                                "not_found",
                                format!(
                                    "file not found: {}; task scratch is removed when its command exits",
                                    original.display()
                                ),
                            ));
                        }
                    }
                }
                self.answer_ask(req_id, original, &identity, operation)?;
                crate::bounded_io::probe_access(req_id, &identity).map(|()| identity)
            }
            AccessDecision::Deny(AccessDenial::ReadOnly) => Err(crate::protocol::Response::error(
                req_id,
                "access_read_only",
                format!(
                    "writes outside the route's roots are denied in read-only mode: {}",
                    original.display()
                ),
            )),
            AccessDecision::Deny(AccessDenial::UnsafeTemp) => {
                Err(crate::protocol::Response::error(
                    req_id,
                    "unsafe_temp_path",
                    format!(
                        "temporary path '{}' is not safe or not owned by this session",
                        original.display()
                    ),
                ))
            }
            AccessDecision::Deny(AccessDenial::Floor) => Err(crate::protocol::Response::error(
                req_id,
                "denied_floor",
                format!(
                    "path '{}' is denied by the access policy floor",
                    original.display()
                ),
            )),
            AccessDecision::Deny(AccessDenial::OutsideRoots | AccessDenial::ScopeGrants(_)) => {
                let root = self
                    .config()
                    .project_root
                    .as_ref()
                    .map(|root| self.resolved_path_restriction_root(root))
                    .unwrap_or_default();
                Err(path_error_response(req_id, original, &root))
            }
        }
    }

    /// Count active LSP server instances.
    pub fn lsp_server_count(&self) -> usize {
        self.lsp_server_count_if_available().unwrap_or(0)
    }

    /// Running language-server count, or `None` when the manager is busy and
    /// the count cannot be read without waiting.
    pub(crate) fn lsp_server_count_if_available(&self) -> Option<usize> {
        self.lsp_manager.try_lock().map(|lsp| lsp.server_count())
    }

    /// Symbol cache statistics from the language provider.
    pub fn symbol_cache_stats(&self) -> serde_json::Value {
        let entries = self
            .symbol_cache
            .read()
            .map(|cache| cache.len())
            .unwrap_or(0);
        serde_json::json!({
            "local_entries": entries,
            "warm_entries": 0,
        })
    }

    fn memory_estimates(&self) -> [crate::memory::MemoryEstimate; 12] {
        #[cfg(test)]
        MEMORY_ESTIMATE_WALKS.with(|walks| walks.set(walks.get() + 1));
        let walked = WalkedMemory {
            measured_at: Instant::now(),
            semantic: self.walk_semantic_memory(),
            trigram: self.walk_trigram_memory(),
            symbols: self.walk_symbol_memory(),
        };
        self.memory_estimates_from(walked)
    }

    /// Shape counters of the semantic index; the outer `None` means its lock
    /// is held (its estimate would then be `busy`, which is never reused).
    fn semantic_memory_shape(&self) -> Option<Option<[u64; 9]>> {
        Some(
            self.semantic_index
                .try_read()
                .ok()?
                .as_ref()
                .map(SemanticIndex::memory_shape),
        )
    }

    /// Shape counters of the in-process trigram index. The index store's
    /// text plane is counted with the store's own memory, so there is none.
    fn trigram_memory_shape(&self) -> Option<Option<[u64; 11]>> {
        Some(None)
    }

    /// Shape counters of the symbol cache; `None` means it is locked.
    fn symbol_memory_shape(&self) -> Option<[u64; 5]> {
        Some(self.symbol_cache.try_read().ok()?.memory_shape())
    }

    /// Walk every entry of the semantic index for its memory estimate.
    fn walk_semantic_memory(&self) -> crate::memory::MemoryEstimate {
        #[cfg(test)]
        MEMORY_INDEX_WALKS.with(|walks| walks.borrow_mut()[0] += 1);
        match self.semantic_index.try_read() {
            Ok(index) => index
                .as_ref()
                .map(SemanticIndex::estimated_memory)
                .unwrap_or_else(|| crate::memory::MemoryEstimate::estimated(0).count("entries", 0)),
            Err(TryLockError::Poisoned(error)) => error
                .into_inner()
                .as_ref()
                .map(SemanticIndex::estimated_memory)
                .unwrap_or_else(|| crate::memory::MemoryEstimate::estimated(0).count("entries", 0)),
            Err(TryLockError::WouldBlock) => crate::memory::MemoryEstimate::busy(),
        }
    }

    /// The in-process trigram index's memory: none, as with its shape.
    fn walk_trigram_memory(&self) -> crate::memory::MemoryEstimate {
        #[cfg(test)]
        MEMORY_INDEX_WALKS.with(|walks| walks.borrow_mut()[1] += 1);
        crate::memory::MemoryEstimate::estimated(0).count("files", 0)
    }

    /// Read the symbol cache's running memory totals without visiting entries.
    fn walk_symbol_memory(&self) -> crate::memory::MemoryEstimate {
        #[cfg(test)]
        MEMORY_INDEX_WALKS.with(|walks| walks.borrow_mut()[2] += 1);
        match self.symbol_cache.try_read() {
            Ok(cache) => cache.estimated_memory(),
            Err(TryLockError::Poisoned(error)) => error.into_inner().estimated_memory(),
            Err(TryLockError::WouldBlock) => crate::memory::MemoryEstimate::busy(),
        }
    }

    /// Build a full memory estimate from index walks (fresh or reused) plus
    /// the cheap subsystem estimates, which are always taken fresh.
    fn memory_estimates_from(&self, walked: WalkedMemory) -> [crate::memory::MemoryEstimate; 12] {
        #[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
        let mut timer = crate::test_allocations::profile::Timer::new();
        let WalkedMemory {
            mut semantic,
            trigram,
            symbols,
            ..
        } = walked;
        let worker_bytes = self.semantic_worker_bytes.load(Ordering::Relaxed);
        if let Some(serving_bytes) = semantic.estimated_bytes {
            semantic
                .counts
                .insert("serving_index_bytes".into(), serving_bytes);
            semantic.estimated_bytes = Some(serving_bytes.saturating_add(worker_bytes));
        } else {
            // A busy serving index must not hide the independently observed worker.
            semantic.estimated_bytes = Some(worker_bytes);
            semantic.bytes_status = "partial";
            semantic
                .not_estimated
                .push("serving_semantic_index_busy".into());
        }
        semantic
            .counts
            .insert("worker_index_bytes".into(), worker_bytes);
        #[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
        timer.mark("memory / live semantic overlay");
        let callgraph =
            crate::memory::MemoryEstimate::estimated(0).count("open_generation_handles", 0);
        let callgraph_projection = self.inspect_manager.callgraph_projection_estimated_memory();
        #[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
        timer.mark("memory / callgraph");
        let inspect = self.inspect_manager.estimated_memory();
        #[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
        timer.mark("memory / inspect");
        let bash = self.bash_background.estimated_memory();
        #[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
        timer.mark("memory / bash");
        let lsp = self
            .lsp_manager
            .try_lock()
            .map(|lsp| lsp.estimated_memory())
            .unwrap_or_else(crate::memory::MemoryEstimate::busy);
        #[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
        timer.mark("memory / lsp");
        // Parsers are created per operation rather than retained in a pool, so
        // parser bytes remain an explicit estimation gap instead of a guess.
        let parser_pool = crate::memory::MemoryEstimate::not_estimated()
            .count("pooled_parsers", 0)
            .gap("tree_sitter_parser_bytes");
        let pending_install = self.pending_install_estimate();
        let exact_memo = self.search_exact_memo.estimated_memory();
        let compression_filters = match self.filter_registry.try_read() {
            Ok(registry) => registry.estimated_memory(),
            Err(TryLockError::Poisoned(error)) => error.into_inner().estimated_memory(),
            Err(TryLockError::WouldBlock) => crate::memory::MemoryEstimate::busy(),
        };
        #[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
        timer.mark("memory / parser and pending install");
        [
            semantic,
            trigram,
            symbols,
            callgraph,
            callgraph_projection,
            inspect,
            bash,
            lsp,
            parser_pool,
            pending_install,
            exact_memo,
            compression_filters,
        ]
    }

    /// Finished semantic loads handed to their receiver but not
    /// yet installed by a completion drain. They are live memory like an
    /// installed index, so they are attributed under their own label rather
    /// than disappearing between hand-off and install.
    fn pending_install_estimate(&self) -> crate::memory::MemoryEstimate {
        let Some(semantic_receiver) = self.semantic_index_rx.try_lock() else {
            return crate::memory::MemoryEstimate::busy();
        };
        let semantic = undelivered_install(
            &self.semantic_pending_install,
            self.semantic_index_rx_epoch(),
            semantic_receiver.as_ref(),
        );
        drop(semantic_receiver);
        let semantic_bytes = semantic.map_or(0, |pending| pending.bytes);
        crate::memory::MemoryEstimate::estimated(semantic_bytes)
            .count("semantic_loads", usize::from(semantic.is_some()))
            .count_u64("semantic_bytes", semantic_bytes)
    }

    /// Build one root's memory estimate using only non-blocking lock attempts.
    /// A contended subsystem is represented as `busy` rather than delaying the
    /// status control path.
    pub fn memory_root_snapshot(&self) -> crate::memory::RootMemorySnapshot {
        let mut estimates = self.memory_estimates();
        let data_layer = Self::fold_data_layer_memory(
            &mut estimates,
            crate::data_layer::surfaces::memory_estimate(self),
        );
        Self::memory_root_snapshot_from(estimates).with_data_layer(data_layer)
    }

    /// Adds the data layer's text, call-graph and meaning bytes to this
    /// root's `search` (trigram), `callgraph` and `semantic` estimates, and
    /// returns what its planes share, the root's `data_layer` estimate; see
    /// [`crate::data_layer::surfaces::DataLayerMemory`]. `None` while the
    /// data layer is off.
    fn fold_data_layer_memory(
        estimates: &mut [crate::memory::MemoryEstimate; 12],
        data_layer: Option<crate::data_layer::surfaces::DataLayerMemory>,
    ) -> Option<crate::memory::MemoryEstimate> {
        let [semantic, trigram, _, callgraph, ..] = estimates;
        data_layer.map(|memory| memory.fold_into(trigram, callgraph, semantic))
    }

    fn memory_root_snapshot_from(
        estimates: [crate::memory::MemoryEstimate; 12],
    ) -> crate::memory::RootMemorySnapshot {
        let [semantic, trigram, symbols, callgraph, callgraph_projection, inspect, bash, lsp, parser_pool, pending_install, exact_memo, compression_filters] =
            estimates;
        crate::memory::RootMemorySnapshot::new(
            semantic,
            trigram,
            symbols,
            callgraph,
            callgraph_projection,
            inspect,
            bash,
            lsp,
            parser_pool,
        )
        .with_pending_install(pending_install)
        .with_query_caches(exact_memo, compression_filters)
    }

    /// Pre-aggregate root memory for capped health diagnostics without building
    /// the rich per-subsystem detail that the status command returns.
    fn memory_root_rollup_from(
        estimates: &[crate::memory::MemoryEstimate; 12],
        data_layer: Option<&crate::memory::MemoryEstimate>,
    ) -> crate::memory::RootMemoryRollup {
        let mut all = estimates.iter().collect::<Vec<_>>();
        all.extend(data_layer);
        crate::memory::RootMemoryRollup::from_estimates(&all)
    }

    /// Return both [`Self::memory_root_snapshot`] and the health rollup's
    /// pre-aggregated root memory, computed from one estimate. Only the
    /// health rollup calls this.
    ///
    /// Semantic and trigram walks visit every entry of the root's resident
    /// indexes; symbol bytes come from running totals. The rollup asks for
    /// every root every few seconds, so each index's estimate is reused while
    /// its shape counters stay the same, for at most
    /// [`HEALTH_MEMORY_REUSE_INTERVAL`]; a change to one index refreshes only
    /// that estimate. The cheap subsystem estimates
    /// are always taken fresh. The rollup's `measured_age_ms` is the age of
    /// the oldest walk in use. A missing index has nothing to walk, so it is
    /// never reused and never makes the figures look older.
    pub(crate) fn memory_root_snapshot_and_rollup(
        &self,
    ) -> (
        crate::memory::RootMemorySnapshot,
        crate::memory::RootMemoryRollup,
    ) {
        #[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
        let mut timer = crate::test_allocations::profile::Timer::new();
        let semantic_shape = self.semantic_memory_shape();
        let trigram_shape = self.trigram_memory_shape();
        let symbol_shape = self.symbol_memory_shape();
        #[cfg(not(test))]
        let now = Instant::now();
        #[cfg(test)]
        let now = memory_walk_now_for_test();
        let mut walks = self.health_memory_walk.lock();
        let mut walked_any = false;
        let mut oldest = now;
        let semantic = if semantic_shape == Some(None) {
            walks.semantic = None;
            crate::memory::MemoryEstimate::estimated(0).count("entries", 0)
        } else {
            let (estimate, measured_at, walked) =
                reuse_or_walk_index_memory(&mut walks.semantic, semantic_shape, || {
                    self.walk_semantic_memory()
                });
            walked_any |= walked;
            oldest = oldest.min(measured_at);
            estimate
        };
        let trigram = if trigram_shape == Some(None) {
            walks.trigram = None;
            crate::memory::MemoryEstimate::estimated(0).count("files", 0)
        } else {
            let (estimate, measured_at, walked) =
                reuse_or_walk_index_memory(&mut walks.trigram, trigram_shape, || {
                    self.walk_trigram_memory()
                });
            walked_any |= walked;
            oldest = oldest.min(measured_at);
            estimate
        };
        let (symbols, measured_at, walked) =
            reuse_or_walk_index_memory(&mut walks.symbols, symbol_shape, || {
                self.walk_symbol_memory()
            });
        walked_any |= walked;
        oldest = oldest.min(measured_at);
        drop(walks);
        #[cfg(test)]
        if walked_any {
            MEMORY_ESTIMATE_WALKS.with(|walks| walks.set(walks.get() + 1));
        }
        #[cfg(not(test))]
        let _ = walked_any;
        let walked = WalkedMemory {
            measured_at: oldest,
            semantic,
            trigram,
            symbols,
        };
        #[cfg(not(test))]
        let age = walked.measured_at.elapsed();
        #[cfg(test)]
        let age = now.saturating_duration_since(walked.measured_at);
        let measured_age_ms = u64::try_from(age.as_millis()).unwrap_or(u64::MAX);
        #[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
        timer.mark("memory / index shape checks and cached estimates");
        let mut estimates = self.memory_estimates_from(walked);
        let data_layer = Self::fold_data_layer_memory(
            &mut estimates,
            crate::data_layer::surfaces::memory_estimate(self),
        );
        let rollup = Self::memory_root_rollup_from(&estimates, data_layer.as_ref())
            .with_measured_age_ms(measured_age_ms);
        (
            Self::memory_root_snapshot_from(estimates).with_data_layer(data_layer),
            rollup,
        )
    }

    /// Attribute all actor roots registered in this process. Standalone mode
    /// has no actor registry, so the current context is inserted directly.
    pub fn memory_snapshot(&self, current_root: Option<&Path>) -> crate::memory::MemorySnapshot {
        self.memory_snapshot_with_cap(current_root, true)
    }

    pub fn memory_snapshot_uncapped(&self) -> crate::memory::MemorySnapshot {
        self.memory_snapshot_with_cap(None, false)
    }

    fn memory_snapshot_with_cap(
        &self,
        current_root: Option<&Path>,
        cap_detail: bool,
    ) -> crate::memory::MemorySnapshot {
        #[cfg(test)]
        self.status_emitter()
            .memory_census_builds
            .fetch_add(1, Ordering::Relaxed);
        let mut roots = BTreeMap::new();
        let (roots_status, contexts) = match self.app.try_memory_contexts() {
            Some(contexts) => ("ready", contexts),
            None => ("busy", Vec::new()),
        };
        for (root, context) in contexts {
            roots.insert(root.display().to_string(), context.memory_root_snapshot());
        }
        // Normalize through the same identity the registry keys on: on Windows
        // a verbatim `\\?\` current root would otherwise land as a SECOND
        // entry for an already-registered root and double-count its memory.
        let current_label = current_root
            .map(|root| {
                cortexkit_paths::ProjectRootId::from_path(root)
                    .map(|id| id.as_path().display().to_string())
                    .unwrap_or_else(|_| root.display().to_string())
            })
            .unwrap_or_else(|| "<unconfigured>".to_string());
        roots
            .entry(current_label)
            .or_insert_with(|| self.memory_root_snapshot());
        if cap_detail {
            crate::memory::MemorySnapshot::new(roots_status, roots)
        } else {
            crate::memory::MemorySnapshot::new_uncapped(roots_status, roots)
        }
    }
}

#[cfg(test)]
mod core_regressions {
    use super::*;

    fn context() -> AppContext {
        AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            Config::default(),
        )
    }

    #[test]
    fn lazy_filter_install_does_not_overwrite_reset() {
        let ctx = context();
        let stale = crate::compress::toml_filter::build_registry(
            &[("stale", "[filter]\nmatches = [\"stale-command\"]\n")],
            None,
            None,
        );
        assert!(stale.lookup("stale-command").is_some());
        ctx.reset_filter_registry();
        ctx.install_lazy_filter_registry(stale);
        assert!(ctx.filter_registry().lookup("stale-command").is_none());
    }

    /// Idle eviction takes the compiled filters; the background bash spawn
    /// path loads them again, because the watchdog's shared handle cannot.
    #[test]
    fn released_filter_registry_is_reloaded_for_background_bash() {
        let ctx = context();
        ctx.reset_filter_registry();
        let handle = Arc::clone(&ctx.filter_registry);
        let builtin = handle.read().unwrap().all().len();
        assert!(builtin > 0, "builtin filters are compiled at reset");

        let taken = ctx.take_filter_registry();
        assert_eq!(taken.all().len(), builtin);
        assert!(!ctx.filter_registry_loaded());
        assert_eq!(
            handle.read().unwrap().all().len(),
            0,
            "the shared slot is emptied"
        );

        let rebuilds = ctx.filter_registry_rebuild_count_for_test();
        ctx.reload_released_filter_registry();
        assert!(ctx.filter_registry_loaded());
        assert_eq!(handle.read().unwrap().all().len(), builtin);
        assert_eq!(ctx.filter_registry_rebuild_count_for_test(), rebuilds + 1);
        // Only a release triggers the spawn-path load.
        ctx.reload_released_filter_registry();
        assert_eq!(ctx.filter_registry_rebuild_count_for_test(), rebuilds + 1);
    }

    /// In the configure timing line (`configure_ack_phase_snapshot`, used by
    /// the slow-configure log and by pending-bind diagnostics), a phase that
    /// marks steps is written as `phase=Nms(step=Nms,...)`. The steps are
    /// listed once the phase has finished and also while its last step is
    /// still running, so a snapshot taken during a stuck configure names the
    /// step it is stuck in.
    #[test]
    fn configure_phase_timing_names_the_steps_of_a_phase() {
        let ctx = context();
        ctx.begin_configure_ack_phase("config_resolve");
        ctx.begin_configure_ack_phase("state_commit");
        ctx.begin_configure_ack_step("database_runtime");
        ctx.begin_configure_ack_step("config_publish");

        let running = ctx.configure_ack_phase_snapshot();
        assert!(
            running.starts_with("config_resolve=")
                && running.contains(",state_commit=")
                && running.contains("ms(database_runtime=")
                && running.contains(",config_publish=")
                && running.ends_with("ms)"),
            "{running}"
        );

        ctx.begin_configure_ack_phase("index_loading_state");
        let finished = ctx.configure_ack_phase_snapshot();
        assert!(
            finished.contains("ms(database_runtime=")
                && finished.contains(",config_publish=")
                && finished.contains("),index_loading_state="),
            "{finished}"
        );
        assert!(
            !finished.contains("index_loading_state=0ms("),
            "steps belong to the phase that marked them: {finished}"
        );
    }

    #[test]
    fn lazy_filter_load_recovers_poison_once() {
        let ctx = context();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = ctx.filter_registry.write().unwrap();
            panic!("poison registry");
        }));
        let _ = ctx.shared_filter_registry();
        let _ = ctx.shared_filter_registry();
        assert_eq!(ctx.filter_registry_rebuild_count_for_test(), 1);
    }

    #[test]
    fn status_context_recovers_poison_for_binding_and_signal() {
        let ctx = Arc::new(context());
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = ctx.status_emitter.context.lock().unwrap();
            panic!("poison weak context");
        }));
        ctx.bind_status_context();
        ctx.signal_status_changed();
    }

    #[test]
    fn config_pin_does_not_attach_to_reused_context_address() {
        let mut slot = Box::new(Some(context()));
        slot.as_ref()
            .as_ref()
            .unwrap()
            .update_config(|config| config.restrict_to_project_root = true);
        let pin = slot.as_ref().as_ref().unwrap().pin_config();
        drop(slot.take());
        *slot = Some(context());
        assert!(
            !slot
                .as_ref()
                .as_ref()
                .unwrap()
                .config()
                .restrict_to_project_root
        );
        drop(pin);
    }

    #[test]
    fn config_publication_refreshes_search_registration() {
        let ctx = context();
        ctx.update_config(|config| config.disabled_tools.push("aft_search".to_string()));
        assert!(!ctx.config().aft_search_registered);
    }

    #[test]
    fn concurrent_publication_keeps_effects_in_snapshot_order() {
        let ctx = Arc::new(context());
        let root_a = tempfile::TempDir::new().unwrap();
        let root_b = tempfile::TempDir::new().unwrap();
        let mut config_a = ctx.config().as_ref().clone();
        config_a.project_root = Some(root_a.path().to_path_buf());
        let mut config_b = ctx.config().as_ref().clone();
        config_b.project_root = Some(root_b.path().to_path_buf());
        let (swapped_tx, swapped_rx) = std::sync::mpsc::channel();
        let (resume_tx, resume_rx) = std::sync::mpsc::channel();
        let worker_ctx = Arc::clone(&ctx);
        let first = std::thread::spawn(move || {
            CONFIG_PUBLICATION_PROBE.with(|probe| {
                *probe.borrow_mut() = Some(Box::new(move || {
                    swapped_tx.send(()).unwrap();
                    resume_rx.recv().unwrap();
                }))
            });
            worker_ctx.set_config(config_a);
        });
        swapped_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let serialized = ctx.config_publication.try_lock().is_none();
        if serialized {
            // A publisher must finish its effects before the next snapshot lands.
            resume_tx.send(()).unwrap();
            first.join().unwrap();
            ctx.set_config(config_b);
        } else {
            // Exercise the stale-effect interleaving when publication lacks exclusion.
            ctx.set_config(config_b);
            resume_tx.send(()).unwrap();
            first.join().unwrap();
        }
        assert!(
            Arc::ptr_eq(
                &ctx.watcher_counters.read().unwrap(),
                &watcher_counters_for_root(root_b.path())
            ),
            "watcher counters must follow the last snapshot"
        );
        assert!(serialized, "snapshot and effects must be one publication");
    }
}

#[cfg(test)]
thread_local! {
    static CONFIG_PUBLICATION_PROBE: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    pub(crate) static STATUS_COUNT_PROBES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Which configure side effect a test counts per (root, session).
#[cfg(test)]
#[derive(Clone, Copy)]
enum ConfigureBindEffect {
    Session,
    Maintenance,
}

#[cfg(test)]
fn configure_bind_effects(
) -> &'static parking_lot::Mutex<BTreeMap<(PathBuf, String), (usize, usize)>> {
    static EFFECTS: std::sync::OnceLock<
        parking_lot::Mutex<BTreeMap<(PathBuf, String), (usize, usize)>>,
    > = std::sync::OnceLock::new();
    EFFECTS.get_or_init(|| parking_lot::Mutex::new(BTreeMap::new()))
}

#[cfg(test)]
fn record_configure_bind_effect_for_test(
    root: &Path,
    session_id: &str,
    effect: ConfigureBindEffect,
) {
    let mut effects = configure_bind_effects().lock();
    let counts = effects
        .entry((root.to_path_buf(), session_id.to_string()))
        .or_default();
    match effect {
        ConfigureBindEffect::Session => counts.0 += 1,
        ConfigureBindEffect::Maintenance => counts.1 += 1,
    }
}

/// How many times configure recorded `session_id` as bound to `root`, and how
/// many configure maintenance jobs it queued for that pair. Tests use roots of
/// their own, so parallel tests do not share entries.
#[cfg(test)]
pub(crate) fn configure_bind_effects_for_test(root: &Path, session_id: &str) -> (usize, usize) {
    configure_bind_effects()
        .lock()
        .get(&(root.to_path_buf(), session_id.to_string()))
        .copied()
        .unwrap_or((0, 0))
}

#[cfg(test)]
mod memory_estimate_tests {
    use super::*;

    #[test]
    fn query_cache_memory_is_attributed_and_released_by_idle_eviction() {
        use crate::commands::semantic_search::{
            comparator::CandidateResult,
            evidence_descriptor::EvidenceDescriptor,
            generation_token::GenerationToken,
            memo::{MemoKey, VerifiedExactSet},
        };
        let ctx = AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            Config::default(),
        );
        let root = tempfile::tempdir().unwrap();
        let key = MemoKey::new(
            root.path(),
            GenerationToken::new_with_str("generation"),
            "query",
            false,
        );
        let warm = || {
            ctx.search_exact_memo
                .get_or_verify(&key, 0, 10, || {
                    Ok(VerifiedExactSet {
                        results: vec![CandidateResult::new_exact(
                            root.path().join("file.rs"),
                            None,
                            EvidenceDescriptor::for_e1(1, true, false),
                        )],
                        file_digests: Default::default(),
                        bound_disclosure: None,
                        stability_void: false,
                    })
                })
                .unwrap();
        };
        ctx.reset_filter_registry();
        warm();
        let (snapshot, rollup) = ctx.memory_root_snapshot_and_rollup();
        assert!(snapshot.exact_memo.estimated_bytes.unwrap() > 0);
        assert!(snapshot.compression_filters.estimated_bytes.unwrap() > 0);
        assert_eq!(snapshot.exact_memo.counts["candidates"], 1);
        assert_eq!(snapshot.attributed_bytes, rollup.attributed_bytes);
        assert!(snapshot.evictable_bytes() >= snapshot.exact_memo.estimated_bytes.unwrap());
        assert!(ctx.evict_idle_artifacts());
        assert_eq!(
            ctx.memory_root_snapshot().exact_memo.estimated_bytes,
            Some(0)
        );
        warm();
        assert!(ctx.try_evict_idle_artifacts());
        assert_eq!(
            ctx.memory_root_snapshot().exact_memo.estimated_bytes,
            Some(0)
        );
    }

    #[test]
    fn symbol_memory_rollup_matches_recount_after_mutations() {
        let ctx = AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            Config::default(),
        );
        // The project root must be absolute on every platform; a bare
        // "/memory-rollup" has no drive letter on Windows.
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("lib.rs");
        let assert_memory = || {
            let expected = ctx.symbol_cache.read().unwrap().recounted_memory_for_test();
            assert_eq!(ctx.memory_root_snapshot().symbols, expected);
            let (snapshot, rollup) = ctx.memory_root_snapshot_and_rollup();
            assert_eq!(snapshot.symbols, expected);
            assert_eq!(rollup.attributed_bytes, snapshot.attributed_bytes);
        };
        assert_memory();
        ctx.symbol_cache.write().unwrap().insert(
            path.clone(),
            SystemTime::UNIX_EPOCH,
            0,
            blake3::hash(b""),
            Vec::new(),
        );
        assert_memory();
        ctx.symbol_cache
            .write()
            .unwrap()
            .set_project_root(root.path().to_path_buf());
        assert_memory();
        ctx.symbol_cache.write().unwrap().invalidate(&path);
        assert_memory();
        ctx.symbol_cache.write().unwrap().reset();
        assert_memory();
    }
}

#[cfg(test)]
mod readonly_artifact_cache_key_tests {
    use super::*;

    #[test]
    fn slow_readonly_identity_derivation_does_not_block_other_roots() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let slow = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let slow_root = std::fs::canonicalize(slow.path()).unwrap();
        let other_root = std::fs::canonicalize(other.path()).unwrap();
        let ctx = Arc::new(AppContext::new(
            default_language_provider_factory(),
            Config::default(),
        ));
        let expected = ctx
            .memoized_readonly_artifact_cache_key(&other_root, storage.path())
            .unwrap();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let slow_ctx = Arc::clone(&ctx);
        let slow_storage = storage.path().to_path_buf();
        let slow_worker = std::thread::spawn(move || {
            slow_ctx.memoized_readonly_artifact_cache_key_with(&slow_root, &slow_storage, || {
                entered_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                crate::root_identity::key::artifact_cache_key_read_only(&slow_root, &slow_storage)
            })
        });
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let other_ctx = Arc::clone(&ctx);
        let other_storage = storage.path().to_path_buf();
        let (finished_tx, finished_rx) = std::sync::mpsc::channel();
        let other_worker = std::thread::spawn(move || {
            finished_tx
                .send(other_ctx.memoized_readonly_artifact_cache_key(&other_root, &other_storage))
                .unwrap();
        });
        let result = finished_rx.recv_timeout(Duration::from_secs(2));
        release_tx.send(()).unwrap();
        slow_worker.join().unwrap().unwrap();
        other_worker.join().unwrap();
        assert_eq!(
            result
                .expect("another root must finish while the first derivation is paused")
                .unwrap(),
            expected
        );
    }
}

#[cfg(test)]
mod git_entry_signature_tests {}

#[cfg(test)]
mod subc_lifecycle_admission_tests {
    use super::*;

    #[test]
    fn health_snapshot_returns_busy_before_locking_artifact_receivers() {
        let ctx = Arc::new(AppContext::new(
            default_language_provider_factory(),
            Config::default(),
        ));
        let lifecycle_guard = ctx.subc_lifecycle.unbound.lock();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (snapshot_tx, snapshot_rx) = std::sync::mpsc::channel();
        let worker_ctx = Arc::clone(&ctx);
        let worker = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            snapshot_tx
                .send(worker_ctx.try_health_snapshot(Path::new("health-root")))
                .unwrap();
        });
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("health snapshot worker should start");

        let snapshot = snapshot_rx.recv_timeout(Duration::from_secs(2));
        drop(lifecycle_guard);
        worker.join().unwrap();

        assert!(
            matches!(
                snapshot,
                Ok(RootHealthSnapshot {
                    state: RootHealthState::Busy,
                    ..
                })
            ),
            "health snapshots must report busy instead of waiting for lifecycle admission"
        );
    }

    #[test]
    fn tier2_health_is_ready_without_a_run_in_main_checkouts_and_worktrees() {
        let root = tempfile::tempdir().unwrap();
        let ctx = AppContext::new(
            default_language_provider_factory(),
            Config {
                project_root: Some(root.path().to_path_buf()),
                ..Config::default()
            },
        );
        ctx.set_harness(crate::harness::Harness::Opencode);
        ctx.set_cache_writer_capabilities(true, true);
        // The whole-repository analyses run only inside aft_inspect, so a
        // writer root with none running is ready to run one.
        assert_eq!(
            ctx.try_health_snapshot(Path::new("writer-root"))
                .tier2
                .expect("tier2 health")
                .status,
            "ready"
        );

        ctx.set_cache_role(true, None);

        // A linked worktree runs them on request too, from its own
        // index-store checkout.
        assert_eq!(
            ctx.try_health_snapshot(Path::new("worktree-root"))
                .tier2
                .expect("tier2 health")
                .status,
            "ready"
        );
        let tier2_snapshot = ctx.tier2_refresh_snapshot().expect("tier2 snapshot");
        assert!(!tier2_snapshot.callgraph_writer);
    }

    #[test]
    fn unbound_artifact_cancellation_clears_semantic_refresh_state() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = AppContext::new(
            default_language_provider_factory(),
            Config {
                project_root: Some(temp.path().to_path_buf()),
                indexes: crate::config::IndexesConfig {
                    trigram: false,
                    semantic: true,
                    callgraph: true,
                },
                ..Config::default()
            },
        );
        *ctx.semantic_index()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(SemanticIndex::new(temp.path().to_path_buf(), 3));
        let mut status = SemanticIndexStatus::ready();
        status.add_refreshing_file(temp.path().join("changed.rs"));
        *ctx.semantic_index_status()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = status;
        let (request_tx, _request_rx) = crossbeam_channel::unbounded();
        let (_event_tx, event_rx) = crossbeam_channel::unbounded();
        ctx.install_semantic_refresh_worker_for_build_epoch(
            request_tx,
            event_rx,
            Arc::new(Mutex::new(None)),
            ctx.semantic_index_rx_epoch(),
        );

        ctx.cancel_unbound_artifact_work();

        assert!(ctx.semantic_refresh_event_rx().lock().is_none());
        assert!(matches!(
            &*ctx
                .semantic_index_status()
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            SemanticIndexStatus::Ready { refreshing, .. } if refreshing.is_empty()
        ));
    }

    #[test]
    fn finished_semantic_refresh_worker_reports_completion_work() {
        let ctx = AppContext::new(default_language_provider_factory(), Config::default());
        let (request_tx, _request_rx) = crossbeam_channel::unbounded();
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        let worker_slot = Arc::new(Mutex::new(Some(std::thread::spawn(|| {}))));
        ctx.install_semantic_refresh_worker_for_build_epoch(
            request_tx,
            event_rx,
            Arc::clone(&worker_slot),
            ctx.semantic_index_rx_epoch(),
        );
        drop(event_tx);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        while !worker_slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .is_some_and(std::thread::JoinHandle::is_finished)
        {
            assert!(
                std::time::Instant::now() < deadline,
                "worker did not finish"
            );
            std::thread::yield_now();
        }

        assert!(
            ctx.completion_drains_have_work(),
            "a finished refresh worker must wake the completion drain after its event queue empties"
        );
    }
}

#[cfg(test)]
mod force_restrict_tests {
    use super::*;
    use crate::language::StubProvider;
    use tempfile::TempDir;

    /// Lay out `main` as a repository and `linked` as one of its worktrees,
    /// the way `git worktree add` does: the worktree's `.git` file names
    /// `main/.git/worktrees/<name>`, which points back at that file and names
    /// its common directory.
    fn link_worktree(main: &Path, linked: &Path) {
        let name = linked.file_name().unwrap();
        let admin = main.join(".git").join("worktrees").join(name);
        std::fs::create_dir_all(&admin).unwrap();
        std::fs::create_dir_all(linked).unwrap();
        std::fs::write(
            linked.join(".git"),
            format!("gitdir: {}\n", admin.display()),
        )
        .unwrap();
        std::fs::write(
            admin.join("gitdir"),
            format!("{}\n", linked.join(".git").display()),
        )
        .unwrap();
        std::fs::write(admin.join("commondir"), "../..\n").unwrap();
    }

    fn plugin_route(owner: &Arc<()>) -> crate::access_policy::AskRouteGuard {
        crate::access_policy::ask_route_guard(Some(crate::access_policy::AskRoute::Plugin {
            scope: crate::access_policy::plugin::Scope::new(owner, "session"),
            once: Vec::new(),
        }))
    }

    /// The main checkout is readable without an ask, but writing there asks:
    /// it is not one of the route's roots.
    #[test]
    fn standalone_main_checkout_is_read_only() {
        let project = tempfile::tempdir_in(crate::test_support::manifest_dir()).unwrap();
        let main = project.path().join("main");
        let linked = project.path().join("linked");
        link_worktree(&main, &linked);
        let target = main.join("source.rs");
        std::fs::write(&target, "fn main_checkout() {}\n").unwrap();
        let ctx = test_context(Some(linked), false);
        ctx.set_cache_role(true, Some(main.join(".git")));
        let owner = Arc::new(());
        let _route = plugin_route(&owner);
        assert!(ctx.validate_read_path("read", "session", &target).is_ok());
        assert_eq!(
            ctx.validate_path("write", &target).unwrap_err().data["code"],
            "permission_required"
        );
    }

    /// A `.git` file is only a claim. One that names a real repository which
    /// does not list this directory as its worktree gives that repository no
    /// exemption.
    #[test]
    fn standalone_forged_worktree_link_gets_no_main_checkout_exemption() {
        let project = tempfile::tempdir_in(crate::test_support::manifest_dir()).unwrap();
        let other = project.path().join("other");
        let real = project.path().join("real-worktree");
        link_worktree(&other, &real);
        let forged = project.path().join("forged");
        std::fs::create_dir(&forged).unwrap();
        std::fs::write(
            forged.join(".git"),
            format!(
                "gitdir: {}\n",
                other.join(".git/worktrees/real-worktree").display()
            ),
        )
        .unwrap();
        let secret = other.join("secret.rs");
        std::fs::write(&secret, "fn other_repository() {}\n").unwrap();
        let ctx = test_context(Some(forged), false);
        ctx.set_cache_role(true, Some(other.join(".git")));
        let owner = Arc::new(());
        let _route = plugin_route(&owner);
        assert_eq!(
            ctx.validate_read_path("forged", "session", &secret)
                .unwrap_err()
                .data["code"],
            "permission_required"
        );
    }

    /// The main checkout is frozen when the role is set. Replacing its path
    /// with a symlink to somewhere else afterwards grants nothing there.
    #[cfg(unix)]
    #[test]
    fn standalone_main_checkout_swapped_for_a_symlink_grants_nothing() {
        let project = tempfile::tempdir_in(crate::test_support::manifest_dir()).unwrap();
        let main = project.path().join("main");
        let linked = project.path().join("linked");
        link_worktree(&main, &linked);
        let ctx = test_context(Some(linked), false);
        ctx.set_cache_role(true, Some(main.join(".git")));
        let elsewhere = project.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        std::fs::write(elsewhere.join("secret.rs"), "fn elsewhere() {}\n").unwrap();
        std::fs::rename(&main, project.path().join("main-moved")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &main).unwrap();
        let owner = Arc::new(());
        let _route = plugin_route(&owner);
        assert_eq!(
            ctx.validate_read_path("swapped", "session", &main.join("secret.rs"))
                .unwrap_err()
                .data["code"],
            "permission_required"
        );
    }

    fn test_context(project_root: Option<PathBuf>, restrict_to_project_root: bool) -> AppContext {
        AppContext::new(
            Box::new(StubProvider),
            Config {
                project_root,
                restrict_to_project_root,
                ..Config::default()
            },
        )
    }

    #[test]
    fn standalone_linked_worktree_keeps_main_checkout_without_prompting() {
        use crate::access_policy::{ask_route_guard, AskRoute};
        let project = tempfile::tempdir_in(crate::test_support::manifest_dir()).unwrap();
        let main = project.path().join("main");
        let linked = project.path().join("linked");
        let unrelated = project.path().join("unrelated.txt");
        link_worktree(&main, &linked);
        let target = main.join("source.rs");
        std::fs::write(&target, "fn main_checkout() {}\n").unwrap();
        std::fs::write(&unrelated, "not the main checkout").unwrap();
        let ctx = test_context(Some(linked), false);
        ctx.set_cache_role(true, Some(main.join(".git")));
        let owner = Arc::new(());
        let scope = crate::access_policy::plugin::Scope::new(&owner, "session");
        let _route = ask_route_guard(Some(AskRoute::Plugin {
            scope,
            once: Vec::new(),
        }));
        assert!(ctx.validate_read_path("main", "session", &target).is_ok());
        let task_temp = project.path().join("task-temp");
        std::fs::create_dir(&task_temp).unwrap();
        let shell = ctx
            .access_policy("shell")
            .unwrap()
            .compile_shell(task_temp, Vec::new(), Vec::new())
            .unwrap();
        let main_identity = crate::access_policy::policy_path(&main);
        assert!(!shell
            .write_allow_roots()
            .iter()
            .any(|root| main_identity.starts_with(root)));
        assert!(!shell
            .data_policy
            .write_allow
            .iter()
            .any(|root| main_identity.starts_with(root)));
        let forced = ctx.with_force_restrict("forced", || {
            ctx.validate_read_path("forced", "session", &target)
        });
        assert_eq!(forced.unwrap_err().data["code"], "path_outside_root");
        assert_eq!(
            ctx.validate_read_path("other", "session", &unrelated)
                .unwrap_err()
                .data["code"],
            "permission_required"
        );
        ctx.update_config(|config| config.restrict_to_project_root = true);
        assert_eq!(
            ctx.validate_read_path("restricted", "session", &target)
                .unwrap_err()
                .data["code"],
            "path_outside_root"
        );
    }

    #[test]
    fn standalone_native_plugin_dispatch_never_uses_lowered() {
        use crate::access_policy::{current_ask_route, with_native_route, AskRoute};
        let root = TempDir::new().unwrap();
        let outside = tempfile::tempdir_in(crate::test_support::manifest_dir()).unwrap();
        let target = outside.path().join("file.txt");
        std::fs::write(&target, "protected").unwrap();
        for harness in [
            crate::harness::Harness::Opencode,
            crate::harness::Harness::Pi,
        ] {
            let ctx = test_context(Some(root.path().into()), false);
            ctx.update_config(|config| config.harness = Some(harness));
            let request = crate::protocol::RawRequest {
                id: "native".into(),
                command: "read".into(),
                session_id: Some("session".into()),
                lsp_hints: None,
                params: serde_json::json!({"path":target}),
            };
            let result = with_native_route(request, &ctx, |req, ctx| {
                assert!(matches!(current_ask_route(), AskRoute::Plugin { .. }));
                ctx.validate_read_path(&req.id, req.session(), &target)
                    .unwrap_err()
            });
            assert_eq!(result.data["code"], "permission_required");
        }
    }

    #[test]
    fn standalone_plugin_ids_do_not_widen_operation_target_or_floor() {
        use crate::access_policy::{ask_route_guard, AskRoute};
        let root = TempDir::new().unwrap();
        let outside = tempfile::tempdir_in(crate::test_support::manifest_dir()).unwrap();
        let target = outside.path().join("file.txt");
        std::fs::write(&target, "unchanged").unwrap();
        let ctx = test_context(Some(root.path().into()), false);
        let owner = Arc::new(());
        let scope = crate::access_policy::plugin::Scope::new(&owner, "session");
        let _route = ask_route_guard(Some(AskRoute::Plugin {
            scope: scope.clone(),
            once: Vec::new(),
        }));
        let read = ctx
            .validate_read_path("read", "session", &target)
            .unwrap_err();
        assert_eq!(read.data["code"], "permission_required");
        let read_id = read.data["asks"][0]["grant_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let _route = ask_route_guard(Some(AskRoute::Plugin {
            scope: scope.clone(),
            once: crate::access_policy::plugin::claim(&mut scope.clone(), &[read_id]),
        }));
        assert!(ctx.validate_read_path("read", "session", &target).is_ok());
        let write = ctx.validate_path("write", &target).unwrap_err();
        assert_eq!(write.data["code"], "permission_required");
        assert_eq!(
            ctx.validate_read_path("other", "session", &outside.path().join("sibling"))
                .unwrap_err()
                .data["code"],
            "permission_required"
        );
        let write_id = write.data["asks"][0]["grant_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let write_grants = crate::access_policy::plugin::claim(&mut scope.clone(), &[write_id]);
        let _route = ask_route_guard(Some(AskRoute::Plugin {
            scope: scope.clone(),
            once: write_grants.clone(),
        }));
        assert!(ctx.validate_path("write", &target).is_ok());
        let changing = outside.path().join("changing");
        std::fs::write(&changing, "file before approval").unwrap();
        let ask = ctx
            .validate_read_path("kind", "session", &changing)
            .unwrap_err();
        let token = ask.data["asks"][0]["grant_id"].as_str().unwrap().to_owned();
        std::fs::remove_file(&changing).unwrap();
        std::fs::create_dir(&changing).unwrap();
        let _route = ask_route_guard(Some(AskRoute::Plugin {
            scope: scope.clone(),
            once: crate::access_policy::plugin::claim(&mut scope.clone(), &[token]),
        }));
        assert_eq!(
            ctx.validate_read_path("kind", "session", &changing)
                .unwrap_err()
                .data["code"],
            "permission_required"
        );
        let _route = ask_route_guard(Some(AskRoute::Plugin {
            scope: scope.clone(),
            once: write_grants,
        }));
        assert!(ctx.validate_path("matching-grant", &target).is_ok());
        ctx.update_config(|config| config.sandbox.read_deny.push(target.clone()));
        assert_eq!(
            ctx.validate_path("floor", &target).unwrap_err().data["code"],
            "denied_floor"
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "unchanged");
    }

    #[test]
    fn standalone_validate_path_parity_without_force_restrict() {
        let root = TempDir::new().expect("root tempdir");
        let outside = TempDir::new().expect("outside tempdir");
        let outside_path = outside.path().join("outside.txt");

        let unrestricted = test_context(Some(root.path().to_path_buf()), false);
        assert_eq!(
            unrestricted
                .validate_path("standalone-unrestricted", &outside_path)
                .expect("unrestricted standalone validates"),
            outside_path
        );

        let restricted = test_context(Some(root.path().to_path_buf()), true);
        let err = restricted
            .validate_path("standalone-restricted", &outside_path)
            .expect_err("restricted standalone rejects outside root");
        assert_eq!(
            serde_json::to_value(err).unwrap()["code"],
            "path_outside_root"
        );
    }

    #[test]
    fn path_restriction_root_memo_canonicalizes_once_for_1000_validations() {
        let root = TempDir::new().expect("root tempdir");
        let target = root.path().join("target.txt");
        std::fs::write(&target, "inside").expect("write target");
        let ctx = test_context(Some(root.path().to_path_buf()), true);

        for request in 0..1_000 {
            let validated = ctx
                .validate_path(&format!("memo-{request}"), &target)
                .expect("in-root path validates");
            assert_eq!(validated, std::fs::canonicalize(&target).unwrap());
        }

        assert_eq!(
            ctx.path_restriction_root_canonicalizations_for_test(),
            1,
            "the configured root should be canonicalized once instead of once per validation"
        );
    }

    #[cfg(unix)]
    #[test]
    fn path_restriction_root_memo_recanonicalizes_after_cached_target_disappears() {
        let workspace = TempDir::new().expect("workspace tempdir");
        let first_target = workspace.path().join("first-target");
        let second_target = workspace.path().join("second-target");
        let configured_root = workspace.path().join("configured-root");
        std::fs::create_dir_all(&first_target).expect("create first target");
        std::fs::create_dir_all(&second_target).expect("create second target");
        std::os::unix::fs::symlink(&first_target, &configured_root)
            .expect("create configured-root symlink");
        std::fs::write(first_target.join("inside.txt"), "first").expect("write first target");

        let ctx = test_context(Some(configured_root.clone()), true);
        assert_eq!(
            ctx.validate_path("first-target", Path::new("inside.txt"))
                .expect("first target validates"),
            std::fs::canonicalize(first_target.join("inside.txt")).unwrap()
        );

        // Keep the configured PathBuf unchanged while replacing its resolved
        // target. The missing cached target must cause a new canonicalization.
        std::fs::remove_dir_all(&first_target).expect("remove first target");
        std::fs::remove_file(&configured_root).expect("remove old root symlink");
        std::os::unix::fs::symlink(&second_target, &configured_root)
            .expect("recreate configured-root symlink");
        std::fs::write(second_target.join("inside.txt"), "second").expect("write second target");

        assert_eq!(
            ctx.validate_path("second-target", Path::new("inside.txt"))
                .expect("second target validates"),
            std::fs::canonicalize(second_target.join("inside.txt")).unwrap()
        );
        assert_eq!(ctx.path_restriction_root_canonicalizations_for_test(), 2);
    }

    #[test]
    fn force_restrict_guard_refcounts_duplicate_request_ids() {
        let root = TempDir::new().expect("root tempdir");
        let outside = TempDir::new().expect("outside tempdir");
        let outside_path = outside.path().join("outside.txt");
        let ctx = test_context(Some(root.path().to_path_buf()), false);

        assert!(ctx.validate_path("dup", &outside_path).is_ok());
        let guard1 = ctx.force_restrict_guard("dup");
        let guard2 = ctx.force_restrict_guard("dup");
        assert!(ctx.validate_path("dup", &outside_path).is_err());
        drop(guard1);
        assert!(
            ctx.validate_path("dup", &outside_path).is_err(),
            "duplicate guard must keep the request over-restricted"
        );
        drop(guard2);
        assert!(ctx.validate_path("dup", &outside_path).is_ok());
    }

    #[test]
    fn with_force_restrict_cleans_up_after_normal_completion_and_panic() {
        let root = TempDir::new().expect("root tempdir");
        let outside = TempDir::new().expect("outside tempdir");
        let outside_path = outside.path().join("outside.txt");
        let ctx = test_context(Some(root.path().to_path_buf()), false);

        ctx.with_force_restrict("normal", || {
            assert!(ctx.validate_path("normal", &outside_path).is_err());
        });
        assert!(!ctx.request_force_restrict("normal"));
        assert!(ctx.validate_path("normal", &outside_path).is_ok());

        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            ctx.with_force_restrict("panic", || {
                assert!(ctx.validate_path("panic", &outside_path).is_err());
                panic!("intentional force-restrict cleanup panic");
            });
        }));
        assert!(panicked.is_err());
        assert!(!ctx.request_force_restrict("panic"));
        assert!(ctx.validate_path("panic", &outside_path).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn validate_write_location_keeps_final_symlink_as_the_authorized_location() {
        let root = TempDir::new().expect("root tempdir");
        let outside = tempfile::NamedTempFile::new().expect("outside file");
        let link = root.path().join("file.txt");
        std::os::unix::fs::symlink(outside.path(), &link).expect("create final symlink");
        let ctx = test_context(Some(root.path().to_path_buf()), false);
        let _guard = ctx.force_restrict_guard("write-location-final-link");

        let validated = ctx
            .validate_write_location("write-location-final-link", &link)
            .expect("the in-root link location is writable");

        assert_eq!(
            validated,
            std::fs::canonicalize(root.path()).unwrap().join("file.txt")
        );
    }

    #[cfg(unix)]
    #[test]
    fn validate_write_location_rejects_symlinked_parent_escape() {
        let root = TempDir::new().expect("root tempdir");
        let outside = TempDir::new().expect("outside tempdir");
        let linked_parent = root.path().join("linked-parent");
        std::os::unix::fs::symlink(outside.path(), &linked_parent).expect("create parent symlink");
        let candidate = linked_parent.join("file.txt");
        let ctx = test_context(Some(root.path().to_path_buf()), false);
        let _guard = ctx.force_restrict_guard("write-location-parent-link");

        let error = ctx
            .validate_write_location("write-location-parent-link", &candidate)
            .expect_err("a symlinked parent must not escape the project root");

        assert_eq!(
            serde_json::to_value(error).unwrap()["code"],
            "path_outside_root"
        );
    }

    #[cfg(unix)]
    #[test]
    fn validate_write_location_rejects_outside_link_to_inside_file() {
        let root = TempDir::new().expect("root tempdir");
        let outside = TempDir::new().expect("outside tempdir");
        let inside = root.path().join("inside.txt");
        std::fs::write(&inside, "inside").unwrap();
        let outside_link = outside.path().join("outside-link.txt");
        std::os::unix::fs::symlink(&inside, &outside_link).expect("create outside symlink");
        let ctx = test_context(Some(root.path().to_path_buf()), false);
        let _guard = ctx.force_restrict_guard("write-location-outside-link");

        let error = ctx
            .validate_write_location("write-location-outside-link", &outside_link)
            .expect_err("an out-of-root lexical location must remain blocked");

        assert_eq!(
            serde_json::to_value(error).unwrap()["code"],
            "path_outside_root"
        );
    }

    #[test]
    fn forced_restrict_without_project_root_fails_closed() {
        let ctx = test_context(None, false);
        let _guard = ctx.force_restrict_guard("missing-root");
        let err = ctx
            .validate_path("missing-root", Path::new("relative.txt"))
            .expect_err("forced restriction without a root must fail closed");
        assert_eq!(
            serde_json::to_value(err).unwrap()["code"],
            "path_outside_root"
        );

        let write_err = ctx
            .validate_write_location("missing-root", Path::new("relative.txt"))
            .expect_err("write-location validation must also fail closed");
        assert_eq!(
            serde_json::to_value(write_err).unwrap()["code"],
            "path_outside_root"
        );
    }
}

#[cfg(test)]
mod callgraph_store_for_ops_tests {
    use super::*;
    use crate::inspect::{InspectCategory, InspectSnapshot, JobOutcome, JobScope};
    use crate::parser::TreeSitterProvider;
    use crate::protocol::RawRequest;
    use serde_json::json;
    use std::path::Path;
    use tempfile::TempDir;

    /// Returns the project and storage directories with the context that uses
    /// them. Callers bind the directories for the whole test, first, so they
    /// are removed when it ends and only after the context is dropped.
    fn cold_build_context() -> ([TempDir; 2], Arc<AppContext>) {
        let project = TempDir::new().expect("project tempdir");
        let storage = TempDir::new().expect("storage tempdir");
        let source_dir = project.path().join("src");
        std::fs::create_dir_all(&source_dir).expect("source dir");
        std::fs::write(
            source_dir.join("lib.rs"),
            "pub fn caller() { callee(); }\npub fn callee() {}\n",
        )
        .expect("source file");

        let ctx = Arc::new(AppContext::new(
            Box::new(TreeSitterProvider::new()),
            Config {
                project_root: Some(project.path().to_path_buf()),
                storage_dir: Some(storage.path().to_path_buf()),
                callgraph_chunk_size: 1,
                ..Config::default()
            },
        ));
        ([project, storage], ctx)
    }

    /// Build a successful call-graph answer and attach its pending-changes
    /// gaps the way dispatch does, with `hook` called at each stage boundary
    /// of the pending-changes capture.
    fn call_tree_answer_with_capture_hook(
        ctx: &AppContext,
        hook: impl FnMut(super::PendingCaptureBoundary) + 'static,
    ) -> crate::protocol::Response {
        let _hook = super::install_pending_capture_hook_for_test(hook);
        let (watcher, callgraph) = crate::response_finalize::query_pending(ctx, "call_tree");
        let mut response = crate::protocol::Response::success("cg", json!({"children": ["stale"]}));
        crate::response_finalize::attach_watcher_query_gap(&mut response, watcher);
        crate::response_finalize::attach_callgraph_pending_gaps(&mut response, callgraph.as_ref());
        response
    }

    fn names_pending_file(response: &crate::protocol::Response, path: &str) -> bool {
        response.data["complete"] == false
            && response.data["gaps"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|gap| gap["path"] == path)
    }

    /// The watcher thread hands a flush window's files to the dispatch
    /// channel and only then clears the window. A flush that lands while an
    /// answer is capturing what is pending must not hide the file from both
    /// reads: the window is read first, so it still names the file.
    #[test]
    fn a_flush_between_two_capture_reads_still_names_the_file() {
        let (_dirs, ctx) = cold_build_context();
        let root = std::fs::canonicalize(ctx.config().project_root.as_ref().unwrap())
            .expect("canonical root");
        ctx.set_canonical_cache_root(root.clone());
        let source = root.join("src/lib.rs");
        let (tx, rx) = crate::watcher_filter::watcher_dispatch_channel();
        *ctx.watcher_rx().lock() = Some(rx);
        let counters = ctx.watcher_counters();
        counters.note_unflushed_paths([&source]);

        let mut flushed = false;
        let response = call_tree_answer_with_capture_hook(&ctx, move |boundary| {
            if boundary != super::PendingCaptureBoundary::AfterFlushWindow || flushed {
                return;
            }
            flushed = true;
            tx.send(WatcherDispatchEvent::Paths(vec![source.clone()]))
                .expect("dispatch the flushed file");
            counters.clear_unflushed_paths();
        });

        assert!(
            names_pending_file(&response, "src/lib.rs"),
            "the answer must be marked incomplete and name the flushed file: {:#}",
            response.data
        );
    }

    #[test]
    fn callgraph_build_start_gates_are_per_root() {
        let first = TempDir::new().expect("first root");
        let second = TempDir::new().expect("second root");
        let (first_reached, first_release) =
            install_callgraph_build_start_gate(first.path().into());
        let (second_reached, second_release) =
            install_callgraph_build_start_gate(second.path().into());
        let workers = [first.path(), second.path()].map(|root| {
            let root = root.to_path_buf();
            std::thread::spawn(move || wait_on_callgraph_build_start_gate(&root))
        });

        first_reached
            .recv_timeout(Duration::from_secs(2))
            .expect("installing a second root's gate must not discard the first root's gate");
        second_reached
            .recv_timeout(Duration::from_secs(2))
            .expect("a held first root must not prevent the second root reaching its gate");
        first_release.send(()).expect("release first root");
        second_release.send(()).expect("release second root");
        for worker in workers {
            worker.join().expect("gated worker");
        }
    }

    fn with_fake_home_env<R>(home: &Path, f: impl FnOnce() -> R) -> R {
        let _guard = crate::test_env::process_env_lock();
        let prev_home = std::env::var_os("HOME");
        let prev_userprofile = std::env::var_os("USERPROFILE");
        unsafe {
            std::env::set_var("HOME", home);
            std::env::set_var("USERPROFILE", home);
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        unsafe {
            match prev_home {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
            match prev_userprofile {
                Some(value) => std::env::set_var("USERPROFILE", value),
                None => std::env::remove_var("USERPROFILE"),
            }
        }
        match result {
            Ok(value) => value,
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }

    fn configure_request_with_params(params: serde_json::Value) -> RawRequest {
        RawRequest {
            id: "cfg".to_string(),
            command: "configure".to_string(),
            lsp_hints: None,
            session_id: None,
            params,
        }
    }

    fn user_tier(doc: serde_json::Value) -> serde_json::Value {
        json!({
            "tier": "user",
            "source": "/u/aft.jsonc",
            "doc": doc.to_string(),
        })
    }

    fn configure_context(project_root: &Path, storage_dir: &Path) -> AppContext {
        let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), Config::default());
        let response = crate::commands::configure::handle_configure(
            &configure_request_with_params(json!({
                "project_root": project_root,
                "harness": "opencode",
                "storage_dir": storage_dir,
                "config": [user_tier(json!({
                    "indexes": { "trigram": true, "semantic": true, "callgraph": true },
                }))],
            })),
            &ctx,
        );
        assert!(response.success, "configure should succeed: {response:?}");
        ctx
    }

    fn inspect_snapshot(ctx: &AppContext) -> InspectSnapshot {
        InspectSnapshot::new(
            ctx.canonical_cache_root(),
            ctx.inspect_dir(),
            ctx.config(),
            ctx.symbol_cache(),
        )
    }

    #[test]
    fn home_root_callers_name_the_home_root_as_the_cause() {
        let home = TempDir::new().expect("home tempdir");
        let storage = TempDir::new().expect("storage tempdir");
        let source_dir = home.path().join("src");
        std::fs::create_dir_all(&source_dir).expect("source dir");
        std::fs::write(
            source_dir.join("lib.rs"),
            "pub fn caller() { callee(); }\npub fn callee() {}\n",
        )
        .expect("source file");

        with_fake_home_env(home.path(), || {
            let ctx = configure_context(home.path(), storage.path());
            let navigation = RawRequest {
                id: "home-callers".to_string(),
                command: "callers".to_string(),
                lsp_hints: None,
                session_id: None,
                params: json!({
                    "file": source_dir.join("lib.rs"),
                    "symbol": "caller",
                }),
            };
            let response = crate::commands::callers::handle_callers(&navigation, &ctx);
            assert!(!response.success);
            // HOME is an unavailable callgraph with a named cause, never a
            // retryable build.
            assert_eq!(response.data["code"], "callgraph_unavailable");
            assert_eq!(
                response.data["index"],
                json!({"callgraph": {"status": "unavailable", "reason": "home_root"}})
            );
            assert!(response.data["results"].is_null());
            assert!(response.data["message"]
                .as_str()
                .is_some_and(|message| message.contains("disabled for home roots")));
        });
    }

    #[test]
    fn home_root_gate_blocks_inspect_manager_submit_paths() {
        let home = TempDir::new().expect("home tempdir");
        let storage = TempDir::new().expect("storage tempdir");
        let source_dir = home.path().join("src");
        std::fs::create_dir_all(&source_dir).expect("source dir");
        std::fs::write(source_dir.join("lib.rs"), "pub fn one() {}\n").expect("source file");

        with_fake_home_env(home.path(), || {
            let ctx = configure_context(home.path(), storage.path());
            let snapshot = inspect_snapshot(&ctx);
            let scope = JobScope::for_project(snapshot.project_root.clone());
            let manager = ctx.inspect_manager();

            assert!(matches!(
                manager.submit_category(snapshot.clone(), InspectCategory::Metrics, scope.clone()),
                JobOutcome::Failed { .. }
            ));

            assert!(matches!(
                manager.tier2_run_with_reuse_blocking(snapshot, InspectCategory::DeadCode, scope),
                JobOutcome::Failed { .. }
            ));
            assert!(
                !manager.tier2_any_in_flight(),
                "HOME root gate must reject Tier-2 submission before any job is queued"
            );
        });
    }

    #[test]
    fn semantic_cold_seed_gate_clear_requests_tier2_pull() {
        let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), Config::default());
        ctx.schedule_semantic_cold_seed_gate_for_configure();

        ctx.resume_deferred_work_after_semantic_cold_seed_gate_cleared();

        assert!(
            !ctx.semantic_cold_seed_active(),
            "retry-wait or cached-load events must reopen the semantic cold gate"
        );
        assert!(
            ctx.tier2_pull_demand_pending(),
            "clearing the semantic cold gate should kick a Tier-2 pull refresh"
        );
    }

    /// Configure resets use the process-wide pacer, so two roots configured in
    /// the same instant do not publish the same cold-refresh deadline.
    #[test]
    fn configure_resets_of_two_roots_get_spaced_cold_refresh_deadlines() {
        let ctx_a = AppContext::new(Box::new(TreeSitterProvider::new()), Config::default());
        let ctx_b = AppContext::new(Box::new(TreeSitterProvider::new()), Config::default());
        let before = Instant::now();
        ctx_a.reset_tier2_refresh_scheduler();
        ctx_b.reset_tier2_refresh_scheduler();
        let deadline = |ctx: &AppContext| {
            ctx.tier2_refresh_scheduler
                .lock()
                .next_dispatch_at(before)
                .expect("a configure publishes a cold-refresh deadline")
        };
        let (first, second) = (deadline(&ctx_a), deadline(&ctx_b));
        assert!(first >= before + crate::inspect::tier2_scheduler::TIER2_REFRESH_COLD_CACHE_DELAY);
        // Other tests configure roots in parallel through the same pacer, so
        // a crowded run may already have pushed reservations to the spread
        // ceiling, where roots share the last slot by design.
        let ceiling = before
            + crate::inspect::tier2_scheduler::TIER2_REFRESH_COLD_CACHE_DELAY
            + crate::inspect::tier2_scheduler::TIER2_COLD_START_MAX_SPREAD;
        assert!(
            second >= first + crate::inspect::tier2_scheduler::TIER2_COLD_START_SPACING
                || second >= ceiling,
            "the second root's cold refresh must be paced after the first"
        );
    }

    /// A whole-repository analysis that waited on another process's inspect
    /// cache lease is not retried in the background: only the next
    /// `aft_inspect` runs it again.
    #[test]
    fn cache_writer_wait_arms_no_background_retry() {
        let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), Config::default());
        let base = Instant::now();
        ctx.note_tier2_refresh_started_at(base);
        ctx.inspect_manager.record_tier2_attempt_outcome_for_test(
            crate::inspect_category::InspectCategory::Complexity,
            crate::inspect::JobOutcome::Failed {
                message: "failed to open inspect cache: writer_lease_timeout: inspect writer lease deadline elapsed".into(),
            },
        );
        assert!(!ctx.tier2_refresh_dispatch_due());
        assert_eq!(
            ctx.tier2_refresh_scheduler.lock().next_dispatch_at(base),
            None,
            "a lease wait must not arm a refresh"
        );
        assert_eq!(
            ctx.tick_tier2_refresh_scheduler_at(base + Duration::from_secs(30 * 60), 0),
            None
        );
    }

    #[test]
    fn semantic_cold_seed_gate_is_per_context_for_tier2_scheduler() {
        let ctx_a = AppContext::new(Box::new(TreeSitterProvider::new()), Config::default());
        let ctx_b = AppContext::new(Box::new(TreeSitterProvider::new()), Config::default());
        let base = Instant::now();
        ctx_a.reset_tier2_refresh_scheduler_at(base);
        ctx_b.reset_tier2_refresh_scheduler_at(base);
        ctx_a.set_semantic_cold_seed_active_for_test(true);

        assert_eq!(
            ctx_a.tick_tier2_refresh_scheduler_at(
                base + crate::inspect::tier2_scheduler::TIER2_REFRESH_COLD_CACHE_DELAY,
                0,
            ),
            None,
            "root A should defer Tier-2 while its semantic cold seed is active"
        );
        assert_eq!(
            ctx_b.tick_tier2_refresh_scheduler_at(
                base + crate::inspect::tier2_scheduler::TIER2_REFRESH_COLD_CACHE_DELAY,
                0,
            ),
            Some(Tier2TriggerReason::ConfigureWarm),
            "root B must not inherit root A's semantic cold gate"
        );
    }

    #[test]
    fn watcher_gap_invalidation_keeps_semantic_reloadable_and_skips_readonly_force_token() {
        let project = TempDir::new().expect("project tempdir");
        let ctx = AppContext::new(
            Box::new(TreeSitterProvider::new()),
            Config {
                project_root: Some(project.path().to_path_buf()),
                indexes: crate::config::IndexesConfig {
                    trigram: false,
                    semantic: true,
                    callgraph: true,
                },
                ..Config::default()
            },
        );
        ctx.set_canonical_cache_root(project.path().to_path_buf());
        // Read-only root: a force token could only be fulfilled by a local
        // writer build, which this root will never run.
        ctx.set_cache_writer_capabilities(false, true);
        *ctx.semantic_index_status()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::ready();

        ctx.invalidate_artifacts_after_watcher_gap();

        assert!(
            matches!(
                &*ctx
                    .semantic_index_status()
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
                SemanticIndexStatus::Ready { .. }
            ),
            "semantic-enabled root must stay reloadable (Disabled has no self-healing path)"
        );
    }

    #[test]
    fn watcher_gap_invalidation_requests_reconcile_for_writer_roots() {
        let project = TempDir::new().expect("project tempdir");
        // Semantic indexing now defaults on; this case exercises the
        // semantic-off mapping, so it switches that index off explicitly.
        let ctx = AppContext::new(
            Box::new(TreeSitterProvider::new()),
            Config {
                project_root: Some(project.path().to_path_buf()),
                indexes: crate::config::IndexesConfig {
                    semantic: false,
                    ..crate::config::IndexesConfig::default()
                },
                ..Config::default()
            },
        );
        ctx.set_canonical_cache_root(project.path().to_path_buf());
        ctx.set_cache_writer_capabilities(true, true);

        ctx.invalidate_artifacts_after_watcher_gap();

        assert!(
            matches!(
                &*ctx
                    .semantic_index_status()
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
                SemanticIndexStatus::Disabled
            ),
            "semantic-disabled config maps to Disabled status"
        );
    }

    #[test]
    fn cancelled_semantic_refresh_transfers_refreshing_files_to_pending() {
        let root = TempDir::new().expect("project tempdir");
        let ctx = AppContext::new(
            Box::new(TreeSitterProvider::new()),
            Config {
                project_root: Some(root.path().to_path_buf()),
                indexes: crate::config::IndexesConfig {
                    trigram: false,
                    semantic: true,
                    callgraph: true,
                },
                ..Config::default()
            },
        );
        *ctx.semantic_index
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(SemanticIndex::new(root.path().to_path_buf(), 3));
        let refreshing_path = root.path().join("src/lib.rs");
        {
            let mut status = ctx
                .semantic_index_status
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *status = SemanticIndexStatus::ready();
            status.start_refreshing_file(refreshing_path.clone());
        }
        let (request_tx, _request_rx) = crossbeam_channel::unbounded();
        let (_event_tx, event_rx) = crossbeam_channel::unbounded();
        ctx.install_semantic_refresh_worker_for_build_epoch(
            request_tx,
            event_rx,
            Arc::new(Mutex::new(None)),
            ctx.semantic_index_rx_epoch(),
        );

        ctx.cancel_unbound_artifact_work();

        // The cancelled worker will never re-embed the in-flight file; the
        // retained pending set is the only record for the replacement worker.
        assert_eq!(
            ctx.pending_semantic_index_paths
                .lock()
                .iter()
                .cloned()
                .collect::<Vec<_>>(),
            vec![refreshing_path],
            "cancelled in-flight refresh files must transfer to the pending set"
        );
        assert!(matches!(
            &*ctx
                .semantic_index_status
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            SemanticIndexStatus::Ready { refreshing, .. } if refreshing.is_empty()
        ));
    }

    #[test]
    fn unbind_before_corpus_started_preserves_corpus_intent() {
        // The probe stamps `refreshing_corpus` before sending, but the worker
        // emits CorpusStarted only after walking the project. An unbind in
        // that window must re-derive the corpus intent from the stamped
        // status, not lose it.
        let root = TempDir::new().expect("project tempdir");
        let ctx = AppContext::new(
            Box::new(TreeSitterProvider::new()),
            Config {
                project_root: Some(root.path().to_path_buf()),
                indexes: crate::config::IndexesConfig {
                    trigram: false,
                    semantic: true,
                    callgraph: true,
                },
                ..Config::default()
            },
        );
        *ctx.semantic_index
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(SemanticIndex::new(root.path().to_path_buf(), 3));
        *ctx.semantic_index_status
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Building {
            stage: "refreshing_corpus".to_string(),
            files: None,
            entries_done: None,
            entries_total: None,
        };
        let (request_tx, _request_rx) = crossbeam_channel::unbounded();
        let (_event_tx, event_rx) = crossbeam_channel::unbounded();
        ctx.install_semantic_refresh_worker_for_build_epoch(
            request_tx,
            event_rx,
            Arc::new(Mutex::new(None)),
            ctx.semantic_index_rx_epoch(),
        );

        ctx.cancel_unbound_artifact_work();

        assert!(
            *ctx.pending_semantic_corpus_refresh.lock(),
            "corpus intent stamped before CorpusStarted must survive the cancellation"
        );
    }

    #[test]
    fn active_semantic_file_refresh_blocks_idle_eviction_until_completion() {
        let root = TempDir::new().expect("project tempdir");
        let ctx = AppContext::new(
            Box::new(TreeSitterProvider::new()),
            Config {
                project_root: Some(root.path().to_path_buf()),
                ..Config::default()
            },
        );
        *ctx.semantic_index
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(SemanticIndex::new(root.path().to_path_buf(), 3));
        let refreshing_path = root.path().join("src/lib.rs");
        {
            let mut status = ctx
                .semantic_index_status
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *status = SemanticIndexStatus::ready();
            status.start_refreshing_file(refreshing_path.clone());
        }

        assert!(ctx.artifact_eviction_blocked());
        assert!(!ctx.evict_idle_artifacts());
        assert!(ctx
            .semantic_index
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some());

        ctx.semantic_index_status
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .complete_refreshing_file(&refreshing_path);
        assert!(ctx.evict_idle_artifacts());
        assert!(ctx
            .semantic_index
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_none());
    }

    /// Eviction releases resident undo metadata without deleting durable history.
    #[test]
    fn backup_memory_idle_eviction_releases_in_memory_backup_stacks() {
        let root = TempDir::new().expect("project tempdir");
        let ctx = AppContext::new(
            Box::new(TreeSitterProvider::new()),
            Config {
                project_root: Some(root.path().to_path_buf()),
                ..Config::default()
            },
        );
        let path = root.path().join("edited.rs");
        std::fs::write(&path, "fn edited() {}\n".repeat(4096)).unwrap();
        ctx.backup()
            .lock()
            .set_storage_dir(root.path().join("storage"), 72);
        ctx.backup()
            .lock()
            .snapshot("session", &path, "before edit")
            .unwrap();

        assert_eq!(ctx.backup().lock().resident_entry_count(), 1);
        assert!(ctx.evict_idle_artifacts());
        assert_eq!(ctx.backup().lock().resident_entry_count(), 0);

        assert_eq!(ctx.backup().lock().history("session", &path).len(), 1);
        assert_eq!(ctx.backup().lock().resident_entry_count(), 0);
        ctx.backup()
            .lock()
            .snapshot("session", &path, "second")
            .unwrap();
        {
            let _held = ctx.backup().lock();
            assert!(
                !ctx.try_evict_idle_artifacts(),
                "frame loop cannot wait for backups"
            );
        }
        assert!(ctx.try_evict_idle_artifacts());
        assert_eq!(ctx.backup().lock().resident_entry_count(), 0);
        assert_eq!(ctx.backup().lock().history("session", &path).len(), 2);
    }
}

#[cfg(test)]
mod status_emitter_tests {
    use super::*;
    use crate::parser::TreeSitterProvider;

    #[test]
    fn status_signal_burst_builds_one_snapshot() {
        let (mut ctx, rx) = ctx_with_frame_rx();
        // Status hydrates checkpoints even before configure. Keep its namespace
        // and file lock private instead of sharing the cwd-wide default store.
        *ctx.checkpoint().lock() = CheckpointStore::unbound_in_for_test(&ctx.storage_dir());
        // Route this emitter to a private debounce loop and queue the whole
        // burst before starting it. Scheduler pauses must not split the
        // signals across debounce windows.
        let (notify, notifications) = mpsc::channel();
        ctx.status_emitter.notify = notify;
        let ctx = Arc::new(ctx);
        ctx.bind_status_context();
        for _ in 0..12 {
            ctx.signal_status_changed();
        }
        assert_eq!(
            ctx.status_emitter.snapshot_builds.load(Ordering::Relaxed),
            0
        );

        let (done_tx, done_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            status_debounce_loop(notifications);
            let _ = done_tx.send(());
        });
        // These deadlines guard hangs, not the duration of a snapshot build.
        let frame = rx
            .recv_timeout(Duration::from_secs(60))
            .expect("status changed");
        assert!(matches!(frame, PushFrame::StatusChanged(_)));
        assert_eq!(
            ctx.status_emitter.snapshot_builds.load(Ordering::Relaxed),
            1
        );
        // Close the input and observe the consumer's exit before checking for
        // extra frames: receiving the first frame alone does not drain a burst.
        drop(ctx);
        done_rx
            .recv_timeout(Duration::from_secs(60))
            .expect("status emitter drained and stopped");
        worker.join().expect("status emitter worker");
        assert!(rx.try_recv().is_err(), "one frame for the entire burst");
    }

    #[test]
    fn status_counts_do_not_acquire_a_held_lsp_manager() {
        let fixture = tempfile::tempdir().unwrap();
        let file = fixture.path().join("main.rs");
        std::fs::write(&file, "fn main() {}\n").unwrap();
        let ctx = Arc::new(AppContext::new(
            Box::new(TreeSitterProvider::new()),
            Config {
                project_root: Some(PathBuf::from("/proj")),
                ..Config::default()
            },
        ));
        ctx.update_status_bar_todos(Some(4));
        ctx.lsp().store_project_applicability(Default::default());
        ctx.lsp().diagnostics_store_mut_for_test().publish(
            crate::lsp::roots::ServerKey {
                kind: crate::lsp::registry::ServerKind::Rust,
                root: PathBuf::from("/proj"),
            },
            file.clone(),
            vec![],
        );
        ctx.lsp().certify_diagnostic_content_for_test(&file);
        let (client, mut fleet_rx) = crate::fleet_status::FleetStatusClient::dial_channel(2);
        ctx.install_fleet_status_client(Some(client));
        let mut first = String::from("first");
        let mut response = crate::protocol::Response::success("status", serde_json::json!({}));
        crate::response_finalize::finalize_tool_response(
            &mut response,
            &mut first,
            &ctx,
            "s",
            "read",
            false,
        );
        assert!(first.contains("T4"));
        assert!(first.contains("E0 W0"));
        fleet_rx
            .try_recv()
            .expect("initial fleet publish")
            .complete_unavailable();
        ctx.update_status_bar_todos(Some(9));
        let held = ctx.lsp();
        let worker_ctx = Arc::clone(&ctx);
        let (tx, rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let counts_unavailable = worker_ctx.try_status_bar_count_values().is_none();
            let push_unavailable = worker_ctx.try_build_status_snapshot().is_none();
            let mut text = String::from("file contents");
            crate::response_finalize::finalize_tool_response(
                &mut response,
                &mut text,
                &worker_ctx,
                "s",
                "read",
                false,
            );
            tx.send((text, counts_unavailable, push_unavailable))
                .unwrap();
        });
        let values = rx.recv_timeout(Duration::from_secs(5));
        drop(held);
        worker.join().unwrap();
        let (text, counts_unavailable, push_unavailable) =
            values.expect("status counts must not acquire a contended manager");
        assert!(counts_unavailable && push_unavailable);
        assert!(
            fleet_rx.try_recv().is_err(),
            "a contended publish must not clear the fleet segment"
        );
        assert_eq!(
            text, "file contents",
            "contention must leave the previous bar alone"
        );
        let mut retry = String::from("next signal");
        let mut response = crate::protocol::Response::success("retry", serde_json::json!({}));
        crate::response_finalize::finalize_tool_response(
            &mut response,
            &mut retry,
            &ctx,
            "s",
            "read",
            false,
        );
        assert!(
            retry.contains("T9"),
            "a skipped emission must not consume the change"
        );
    }

    /// A context for `root` whose status pushes arrive on the returned
    /// receiver, registered the way a transport root is.
    fn bound_ctx_with_frame_rx(root: &Path) -> (Arc<AppContext>, mpsc::Receiver<PushFrame>) {
        let ctx = Arc::new(AppContext::new(
            Box::new(TreeSitterProvider::new()),
            Config {
                project_root: Some(root.to_path_buf()),
                ..Config::default()
            },
        ));
        let (tx, rx) = mpsc::channel();
        ctx.set_progress_sender(Some(Arc::new(Box::new(move |frame| {
            let _ = tx.send(frame);
        }))));
        ctx.bind_status_context();
        (ctx, rx)
    }

    /// Building one project's status can touch its filesystem, which may sit
    /// on a slow or unavailable mount. While that build waits, another
    /// project's status must still go out within its debounce window.
    #[test]
    fn a_stalled_status_build_does_not_delay_another_context() {
        let slow_root = tempfile::tempdir().unwrap();
        let fast_root = tempfile::tempdir().unwrap();
        let (slow, slow_rx) = bound_ctx_with_frame_rx(slow_root.path());
        let (fast, fast_rx) = bound_ctx_with_frame_rx(fast_root.path());
        let gate = gate_status_emit_for_test(slow_root.path());

        slow.status_emitter().signal(slow.build_status_snapshot());
        gate.reached
            .recv_timeout(Duration::from_millis(STATUS_DEBOUNCE_MS + 5_000))
            .expect("the slow project's status build started");
        fast.status_emitter().signal(fast.build_status_snapshot());
        // The slow build stays gated until released below, so a fast status that
        // waited for it would never arrive; a generous bound therefore cannot
        // hide that bug. It only absorbs a loaded Windows runner, where the fast
        // build alone has taken over 2 s.
        let fast_frame = fast_rx.recv_timeout(Duration::from_millis(STATUS_DEBOUNCE_MS + 10_000));
        // Release before asserting, so a failure does not leave the shared
        // status machinery blocked for other tests.
        let _ = gate.release.send(());
        assert!(
            matches!(fast_frame, Ok(PushFrame::StatusChanged(_))),
            "another project's status waited for the stalled build"
        );
        assert!(
            matches!(
                slow_rx.recv_timeout(Duration::from_secs(10)),
                Ok(PushFrame::StatusChanged(_))
            ),
            "the stalled project's status goes out once its build finishes"
        );
    }

    fn ctx_with_frame_rx() -> (AppContext, mpsc::Receiver<PushFrame>) {
        let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), Config::default());
        let (tx, rx) = mpsc::channel();
        ctx.set_progress_sender(Some(Arc::new(Box::new(move |frame| {
            let _ = tx.send(frame);
        }))));
        (ctx, rx)
    }

    #[test]
    fn status_emitter_signal_triggers_push() {
        let (ctx, rx) = ctx_with_frame_rx();
        ctx.status_emitter().signal(ctx.build_status_snapshot());
        let frame = rx
            .recv_timeout(Duration::from_millis(STATUS_DEBOUNCE_MS + 500))
            .expect("status_changed push");
        assert!(matches!(frame, PushFrame::StatusChanged(_)));
    }

    #[test]
    fn status_emitter_debounces_burst() {
        let (ctx, rx) = ctx_with_frame_rx();
        // The claim is "signals inside one window produce one push", so the
        // burst must provably fit inside the window: a loaded CI runner has
        // stretched ten signals past 1 s and read the second, legitimate push
        // as a debounce failure (train 117). Measure the burst and retry it
        // until it fits; only then does the single-push assertion mean anything.
        let mut attempts = 0;
        loop {
            let started = Instant::now();
            for _ in 0..10 {
                ctx.status_emitter().signal(ctx.build_status_snapshot());
            }
            if started.elapsed() < Duration::from_millis(STATUS_DEBOUNCE_MS / 2) {
                break;
            }
            attempts += 1;
            assert!(
                attempts < 20,
                "could not issue ten signals inside half a debounce window"
            );
            // Drain whatever the slow burst produced before trying again.
            while rx
                .recv_timeout(Duration::from_millis(STATUS_DEBOUNCE_MS + 500))
                .is_ok()
            {}
        }
        let frame = rx
            .recv_timeout(Duration::from_millis(STATUS_DEBOUNCE_MS + 500))
            .expect("status_changed push");
        assert!(matches!(frame, PushFrame::StatusChanged(_)));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn status_emitter_separate_windows_separate_pushes() {
        let (ctx, rx) = ctx_with_frame_rx();
        ctx.status_emitter().signal(ctx.build_status_snapshot());
        rx.recv_timeout(Duration::from_millis(STATUS_DEBOUNCE_MS + 500))
            .expect("first push");
        ctx.status_emitter().signal(ctx.build_status_snapshot());
        rx.recv_timeout(Duration::from_millis(STATUS_DEBOUNCE_MS + 500))
            .expect("second push");
    }

    #[test]
    fn status_emitter_no_signal_no_push() {
        let (_ctx, rx) = ctx_with_frame_rx();
        assert!(rx
            .recv_timeout(Duration::from_millis(STATUS_DEBOUNCE_MS + 100))
            .is_err());
    }

    #[test]
    fn status_emitter_dropped_context_pushes_nothing() {
        // A context dropped inside its open debounce window must not push.
        let (ctx, rx) = ctx_with_frame_rx();
        ctx.status_emitter().signal(ctx.build_status_snapshot());
        drop(ctx);
        assert!(rx
            .recv_timeout(Duration::from_millis(STATUS_DEBOUNCE_MS + 300))
            .is_err());
    }

    #[test]
    fn status_emitter_windows_are_per_context() {
        // Contexts share one debounce thread; a burst on one context must not
        // absorb another context's push.
        let (first, first_rx) = ctx_with_frame_rx();
        let (second, second_rx) = ctx_with_frame_rx();
        for _ in 0..5 {
            first.status_emitter().signal(first.build_status_snapshot());
            second
                .status_emitter()
                .signal(second.build_status_snapshot());
        }
        for rx in [&first_rx, &second_rx] {
            let frame = rx
                .recv_timeout(Duration::from_millis(STATUS_DEBOUNCE_MS + 2_000))
                .expect("each context pushes its own status");
            assert!(matches!(frame, PushFrame::StatusChanged(_)));
        }
    }

    #[test]
    fn progress_sender_slot_is_per_context_for_shared_app() {
        let app = App::default_shared();
        let ctx_a = AppContext::from_app(Arc::clone(&app), Config::default());
        let ctx_b = AppContext::from_app(app, Config::default());
        let (tx_a, rx_a) = mpsc::channel();
        let (tx_b, rx_b) = mpsc::channel();

        ctx_a.set_progress_sender(Some(Arc::new(Box::new(move |frame| {
            let _ = tx_a.send(frame);
        }))));
        ctx_b.set_progress_sender(Some(Arc::new(Box::new(move |frame| {
            let _ = tx_b.send(frame);
        }))));

        ctx_a.emit_progress(ProgressFrame {
            frame_type: "progress",
            request_id: "ctx-a".to_string(),
            kind: crate::protocol::ProgressKind::Stdout,
            chunk: "a".to_string(),
            origin: None,
        });
        ctx_b.emit_progress(ProgressFrame {
            frame_type: "progress",
            request_id: "ctx-b".to_string(),
            kind: crate::protocol::ProgressKind::Stdout,
            chunk: "b".to_string(),
            origin: None,
        });

        match rx_a
            .recv_timeout(Duration::from_millis(50))
            .expect("ctx A progress frame")
        {
            PushFrame::Progress(frame) => assert_eq!(frame.request_id, "ctx-a"),
            other => panic!("unexpected frame for ctx A: {other:?}"),
        }
        assert!(rx_a.try_recv().is_err());

        match rx_b
            .recv_timeout(Duration::from_millis(50))
            .expect("ctx B progress frame")
        {
            PushFrame::Progress(frame) => assert_eq!(frame.request_id, "ctx-b"),
            other => panic!("unexpected frame for ctx B: {other:?}"),
        }
        assert!(rx_b.try_recv().is_err());
    }
}

#[cfg(test)]
mod health_warming_honesty_tests {
    use super::*;
    use crate::parser::TreeSitterProvider;

    fn ctx_with_config(config: Config) -> AppContext {
        AppContext::new(Box::new(TreeSitterProvider::new()), config)
    }

    fn health_tier2_status(ctx: &AppContext) -> &'static str {
        let root = std::path::Path::new("/tmp/health-warming-honesty-test");
        ctx.try_health_snapshot(root)
            .tier2
            .expect("tier2 component present")
            .status
    }

    #[test]
    fn health_tier2_and_inspect_builder_state_read_the_same_registry() {
        // Health must not report ready while the inspect builder registry
        // still has a live registration for this root.
        let ctx = ctx_with_config(Config::default());
        ctx.inspect_manager()
            .set_tier2_in_flight_for_test(crate::inspect_category::InspectCategory::DeadCode, true);

        assert_eq!(health_tier2_status(&ctx), "building");
        assert_eq!(
            ctx.inspect_manager()
                .tier2_builder_state(crate::inspect_category::InspectCategory::DeadCode),
            crate::inspect::InspectBuilderState::Building
        );

        ctx.inspect_manager().set_tier2_in_flight_for_test(
            crate::inspect_category::InspectCategory::DeadCode,
            false,
        );

        assert_eq!(health_tier2_status(&ctx), "ready");
        assert_eq!(
            ctx.inspect_manager()
                .tier2_builder_state(crate::inspect_category::InspectCategory::DeadCode),
            crate::inspect::InspectBuilderState::Absent
        );

        ctx.inspect_manager().record_tier2_attempt_outcome_for_test(
            crate::inspect_category::InspectCategory::DeadCode,
            crate::inspect::JobOutcome::Fresh {
                payload: crate::inspect::scanners::dead_code::callgraph_unavailable_aggregate(0),
            },
        );
        assert_eq!(
            health_tier2_status(&ctx),
            "ready",
            "a finished callgraph_unavailable attempt must not keep health.tier2=building"
        );
        assert_eq!(
            ctx.inspect_manager()
                .tier2_builder_state(crate::inspect_category::InspectCategory::DeadCode),
            crate::inspect::InspectBuilderState::Absent
        );
        assert!(
            ctx.inspect_manager()
                .tier2_builder_state_detail(crate::inspect_category::InspectCategory::DeadCode)
                .starts_with("last attempt failed: callgraph_unavailable (attempt 1, first at "),
            "inspect refusals must carry the failed-attempt history the health surface no longer treats as busy"
        );
    }

    /// Nothing refreshes the whole-repository analyses in the background, so
    /// edits never leave the plane stale with a refresh deadline: it reports
    /// ready, with only its status field.
    #[test]
    fn edits_leave_the_tier2_plane_ready_without_refresh_fields() {
        let ctx = ctx_with_config(Config::default());
        let now = Instant::now();
        ctx.note_tier2_refresh_started_at(now);
        assert_eq!(ctx.tick_tier2_refresh_scheduler_at(now, 1), None);
        ctx.add_pending_tier2_paths([PathBuf::from("src/lib.rs")]);

        let tier2 = serde_json::to_value(
            ctx.try_health_snapshot(Path::new("/tmp/health-warming-honesty-test"))
                .tier2
                .expect("tier2 component present"),
        )
        .expect("tier2 health serializes");

        assert_eq!(tier2, serde_json::json!({ "status": "ready" }));
        assert!(!ctx.inspect_manager().tier2_any_in_flight());
    }

    #[test]
    fn failed_tier2_pass_returns_the_plane_to_ready() {
        let ctx = ctx_with_config(Config::default());
        ctx.inspect_manager().set_tier2_in_flight_for_test(
            crate::inspect_category::InspectCategory::Duplicates,
            true,
        );
        assert_eq!(health_tier2_status(&ctx), "building");

        ctx.inspect_manager().record_tier2_attempt_outcome_for_test(
            crate::inspect_category::InspectCategory::Duplicates,
            crate::inspect::JobOutcome::Failed {
                message: "scan failed".to_string(),
            },
        );
        assert_eq!(health_tier2_status(&ctx), "ready");
    }
}

#[cfg(test)]
mod view_load_health_tests {
    use super::*;
    use crate::parser::TreeSitterProvider;

    fn ctx() -> AppContext {
        AppContext::new(Box::new(TreeSitterProvider::new()), Config::default())
    }

    #[test]
    fn view_load_log_throttle_reports_suppressed_repeats_per_project_and_plane() {
        let ctx = ctx();
        let other_project = AppContext::new(Box::new(TreeSitterProvider::new()), Config::default());
        let error = "view I/O failed reading /project/a.ts: Permission denied";
        let ((), first_lines) = crate::logging::capture_log_lines(|| {
            ctx.note_view_load_failure("trigram", error);
            ctx.note_view_load_failure("trigram", error);
            ctx.note_view_load_failure("trigram", error);
            ctx.note_view_load_failure("callgraph", error);
            other_project.note_view_load_failure("trigram", error);
        });
        assert_eq!(first_lines.len(), 3, "{first_lines:?}");
        assert!(first_lines
            .iter()
            .all(|line| line.starts_with("ERROR ") && line.contains("suppressed=0")));
        assert_eq!(ctx.view_load_suppressed_for_test("trigram"), 2);
        assert_eq!(ctx.view_load_suppressed_for_test("callgraph"), 0);
        assert_eq!(other_project.view_load_suppressed_for_test("trigram"), 0);

        // Expire only the throttle interval, without sleeping for a minute.
        {
            let mut health = ctx.view_load_health.lock();
            let (last_log, _) = health.log_state.get_mut("trigram").unwrap();
            *last_log = Some(Instant::now() - VIEW_LOAD_LOG_INTERVAL);
        }
        let ((), next_lines) = crate::logging::capture_log_lines(|| {
            ctx.note_view_load_failure("trigram", error);
        });
        assert_eq!(next_lines.len(), 1, "{next_lines:?}");
        assert!(next_lines[0].contains("plane=trigram"));
        assert!(next_lines[0].contains("failures=4"));
        assert!(next_lines[0].contains("suppressed=2"));
        assert_eq!(ctx.view_load_suppressed_for_test("trigram"), 0);
    }
}

#[cfg(test)]
mod configure_applicability_tests {
    use super::*;

    #[test]
    fn applicability_walk_does_not_retain_released_storage_leases() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        let storage = temp.path().join("storage");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&storage).unwrap();
        let mut config = Config {
            project_root: Some(root),
            storage_dir: Some(storage.clone()),
            ..Config::default()
        };
        crate::root_cache::ensure_config_storage_lease(&mut config).unwrap();
        assert!(crate::root_cache::prepare_cache_root(&mut config).is_none());
        let cache_lease = Arc::downgrade(config.cache_lease.as_ref().unwrap());
        let storage_lease = Arc::downgrade(config.storage_lease.as_ref().unwrap());
        let ctx = AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            config.clone(),
        );
        let _held_walk = ctx.hold_project_applicability_walks_for_test();
        ctx.refresh_project_applicability(Arc::new(config), false);

        #[cfg(target_os = "linux")]
        let open_under_storage = || {
            std::fs::read_dir("/proc/self/fd")
                .unwrap()
                .filter_map(Result::ok)
                .filter_map(|entry| std::fs::read_link(entry.path()).ok())
                .filter(|target| target.starts_with(&storage))
                .collect::<Vec<_>>()
        };
        #[cfg(target_os = "linux")]
        assert!(!open_under_storage().is_empty(), "live context owns leases");
        drop(ctx);
        #[cfg(target_os = "linux")]
        {
            let targets = open_under_storage();
            assert!(
                targets.is_empty(),
                "released storage retained descriptors: {targets:?}"
            );
        }
        assert!(
            cache_lease.upgrade().is_none(),
            "cache lease released synchronously"
        );
        assert!(
            storage_lease.upgrade().is_none(),
            "storage lease released synchronously"
        );
    }

    #[test]
    fn configure_applicability_content_batches_keep_cache_and_health_counters() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let source = root.join("main.py");
        std::fs::write(&source, "print(0)").unwrap();
        let config = Config {
            project_root: Some(root.clone()),
            ..Config::default()
        };
        let ctx = Arc::new(AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            config.clone(),
        ));
        ctx.refresh_project_applicability(Arc::new(config.clone()), false);
        assert!(ctx.wait_for_project_applicability(Duration::from_secs(10)));
        let spawns = ctx
            .project_applicability_progress
            .refresh_spawns
            .load(Ordering::SeqCst);
        let before = crate::lsp::manager::applicability_metrics(&root).full_walks;
        for n in 1..=50 {
            std::fs::write(&source, format!("print({n})")).unwrap();
            ctx.lsp_note_watcher_batch(std::slice::from_ref(&source), true);
            crate::lsp::manager::walk_applicable_area(&root, None, &config, None).unwrap();
        }
        assert_eq!(
            ctx.project_applicability_progress
                .refresh_spawns
                .load(Ordering::SeqCst),
            spawns
        );
        let census = crate::lifecycle_census::collect(&App::default(), &[ctx]);
        let health = serde_json::to_value(census).unwrap();
        // The census keys roots by their normalized form, which on Windows has
        // no `\\?\` prefix; `fs::canonicalize` above returns the prefixed form.
        let key = crate::path_identity::canonicalize_normalized(&root)
            .display()
            .to_string();
        let metrics = &health["lsp"]["applicability_by_root"][key];
        assert_eq!(metrics["full_walks"].as_u64().unwrap() - before, 0);
        assert_eq!(metrics["retained_content_batches"], 50);
        assert_eq!(metrics["hits"], 50);
        assert!(metrics["dependency_count"].as_u64().unwrap() > 0);
        assert!(metrics["warm_validation_micros"].is_u64());
        assert!(metrics["full_walk_micros"].is_u64());
    }

    #[test]
    fn configure_applicability_coalesces_watcher_batches_during_scan() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let config = Config {
            project_root: Some(root.clone()),
            ..Config::default()
        };
        let ctx = AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            config.clone(),
        );
        let progress = &ctx.project_applicability_progress;
        progress.scans.lock().held = true;
        ctx.refresh_project_applicability(Arc::new(config), false);
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut scans = progress.scans.lock();
        while scans.started == 0 {
            assert!(!progress
                .scan_changed
                .wait_until(&mut scans, deadline)
                .timed_out());
        }
        drop(scans);
        for _ in 0..32 {
            ctx.lsp_forward_watcher_file_events(&[root.join("main.go")]);
        }
        // Always release the worker before assertions, including on the old
        // implementation where every batch starts a separate waiting thread.
        progress.scans.lock().held = false;
        progress.scan_changed.notify_all();
        assert!(ctx.wait_for_project_applicability(Duration::from_secs(10)));
        assert_eq!(
            progress.refresh_spawns.load(Ordering::SeqCst),
            1,
            "watcher batches must reuse the active refresh worker"
        );
        assert_eq!(
            progress.scans.lock().started,
            2,
            "a batch burst needs only the held scan and one pending scan"
        );
    }
}

#[cfg(test)]
mod status_bar_tests {
    use super::*;
    use crate::parser::TreeSitterProvider;

    fn ctx() -> AppContext {
        let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), Config::default());
        // These count-cache fixtures inject reports without filesystem or server
        // discovery; their project-wide applicability set is explicitly empty.
        ctx.lsp().store_project_applicability(Default::default());
        ctx
    }

    #[test]
    fn diagnostics_counts_are_unknown_until_project_applicability_exists() {
        let fixture = tempfile::tempdir().unwrap();
        let file = fixture.path().join("main.rs");
        std::fs::write(&file, "fn main() {}\n").unwrap();
        let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), Config::default());
        ctx.lsp()
            .diagnostics_store_mut_for_test()
            .publish_with_kind(
                crate::lsp::registry::ServerKind::Rust,
                file.clone(),
                Vec::new(),
            );
        ctx.lsp().certify_diagnostic_content_for_test(&file);
        assert_eq!(
            (
                ctx.status_bar_count_values().errors,
                ctx.status_bar_count_values().warnings
            ),
            (None, None)
        );
        ctx.lsp().store_project_applicability(Default::default());
        assert_eq!(
            (
                ctx.status_bar_count_values().errors,
                ctx.status_bar_count_values().warnings
            ),
            (Some(0), Some(0))
        );
    }

    #[test]
    fn status_bar_never_promotes_a_masked_zero_into_a_warm_certificate() {
        use crate::lsp::diagnostics::{DiagnosticSeverity, StoredDiagnostic};
        use crate::lsp::registry::ServerKind;
        use crate::lsp::roots::ServerKey;
        let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), Config::default());
        let file = PathBuf::from("/fixture/main.py");
        let key = ServerKey {
            kind: ServerKind::Python,
            root: PathBuf::from("/fixture"),
        };
        ctx.lsp()
            .diagnostics_store_mut_for_test()
            .publish(key.clone(), file.clone(), Vec::new());
        assert_eq!(ctx.status_bar_count_values().errors, None);
        ctx.lsp()
            .diagnostics_store_mut_for_test()
            .publish_full_with_provisional(
                key,
                file.clone(),
                vec![StoredDiagnostic {
                    file,
                    line: 1,
                    column: 1,
                    end_line: 1,
                    end_column: 2,
                    severity: DiagnosticSeverity::Error,
                    message: "not certified yet".into(),
                    code: None,
                    source: None,
                }],
                None,
                None,
                true,
            );
        ctx.lsp().store_project_applicability(Default::default());
        let counts = ctx.status_bar_count_values();
        assert_eq!(
            (counts.errors, counts.warnings),
            (None, None),
            "a zero hidden behind unknown applicability was never a certified warm pair"
        );
    }

    #[test]
    fn truthful_values_omit_unproven_categories_and_legacy_projection_requires_all_counts() {
        let ctx = ctx();
        let values = ctx.status_bar_count_values();
        assert_eq!(values.errors, None);
        assert_eq!(values.warnings, None);
        assert_eq!(values.todos, None);
        assert!(ctx.status_bar_counts().is_none());

        ctx.update_status_bar_todos(Some(2));
        let values = ctx.status_bar_count_values();
        assert_eq!(values.todos, Some(2));
        assert_eq!(values.errors, None, "no analyzer report is not a clean E0");
        assert_eq!(
            values.warnings, None,
            "no analyzer report is not a clean W0"
        );

        assert_eq!(
            ctx.status_bar_counts(),
            None,
            "the legacy numeric shape must not fabricate missing diagnostics"
        );
    }

    #[test]
    fn changing_root_clears_project_scoped_status_counts() {
        let temp = tempfile::tempdir().expect("tempdir");
        let first_root = temp.path().join("first");
        let second_root = temp.path().join("second");
        std::fs::create_dir_all(&first_root).expect("create first root");
        std::fs::create_dir_all(&second_root).expect("create second root");
        let ctx = ctx();
        ctx.set_canonical_cache_root(first_root);
        ctx.update_status_bar_todos(Some(2));
        assert_eq!(ctx.status_bar_count_values().todos, Some(2));

        ctx.set_canonical_cache_root(second_root);

        let values = ctx.status_bar_count_values();
        assert_eq!(values.todos, None);
        assert!(
            ctx.status_bar_counts().is_none(),
            "counts from the previous root must not appear in a newly bound root"
        );
    }

    #[test]
    fn cache_hit_preserves_omissions() {
        let ctx = ctx();
        let first = ctx.status_bar_count_values();
        assert_eq!(first.todos, None);
        assert!(ctx.status_bar_counts().is_none());

        let cached = ctx.status_bar_count_values();
        assert_eq!(cached, first, "a cache hit must preserve every omission");
        let cache = ctx
            .status_bar_cached
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(cache.valid);
        assert_eq!(cache.counts.as_ref(), Some(&first));
        drop(cache);

        ctx.update_status_bar_todos(Some(3));
        assert_eq!(ctx.status_bar_count_values().todos, Some(3));
    }

    #[test]
    fn update_with_none_todos_preserves_last_known_todos() {
        let ctx = ctx();
        ctx.update_status_bar_todos(Some(9));
        // A refresh that produced no TODO count keeps the last known one.
        ctx.update_status_bar_todos(None);
        let counts = ctx.status_bar_count_values();
        assert_eq!(counts.todos, Some(9));
    }

    // End-to-end wiring: a diagnostic for a file inflates the status-bar `E`
    // count (read live from the warm LSP set); clearing that file's diagnostics
    // (the deleted-file path) drops it back. This is the AppContext glue between
    // the watcher-drain clear and the agent-visible bar.
    #[test]
    fn clearing_diagnostics_for_deleted_file_drops_status_bar_errors() {
        use crate::lsp::diagnostics::{DiagnosticSeverity, StoredDiagnostic};
        use crate::lsp::registry::ServerKind;
        use crate::lsp::roots::ServerKey;

        let ctx = ctx();
        ctx.update_status_bar_todos(Some(0)); // populate so the bar surfaces

        let fixture = tempfile::tempdir().unwrap();
        let file = fixture.path().join("gone.ts");
        std::fs::write(&file, "const value = 1;\n").unwrap();
        {
            let mut lsp = ctx.lsp();
            lsp.diagnostics_store_mut_for_test().publish(
                ServerKey {
                    kind: ServerKind::TypeScript,
                    root: std::path::PathBuf::from("/proj"),
                },
                file.clone(),
                vec![StoredDiagnostic {
                    file: file.clone(),
                    line: 1,
                    column: 1,
                    end_line: 1,
                    end_column: 2,
                    severity: DiagnosticSeverity::Error,
                    message: "boom".into(),
                    code: None,
                    source: None,
                }],
            );
            lsp.certify_diagnostic_content_for_test(&file);
        }

        // Bar reflects the live warm-set error.
        assert_eq!(ctx.status_bar_count_values().errors, Some(1));

        // Clearing the (now-deleted) file's diagnostics drops the count.
        let removed = ctx.lsp_clear_diagnostics_for_file(&file);
        assert!(removed);
        assert_eq!(ctx.status_bar_count_values().errors, None);
    }

    #[test]
    fn status_bar_preserves_authoritative_counts_until_provisional_report_is_promoted() {
        use crate::lsp::diagnostics::{DiagnosticSeverity, StoredDiagnostic};
        use crate::lsp::registry::ServerKind;
        use crate::lsp::roots::ServerKey;

        let ctx = ctx();
        ctx.update_status_bar_todos(Some(0));
        let fixture = tempfile::tempdir().unwrap();
        let root = crate::path_identity::canonicalize_normalized(fixture.path());
        let file = root.join("src/main.rs");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "fn main() {}\n").unwrap();
        let key = ServerKey {
            kind: ServerKind::Rust,
            root,
        };
        let diagnostic = |severity, message: &str| StoredDiagnostic {
            file: file.clone(),
            line: 1,
            column: 1,
            end_line: 1,
            end_column: 2,
            severity,
            message: message.into(),
            code: None,
            source: None,
        };

        {
            let mut lsp = ctx.lsp();
            lsp.diagnostics_store_mut_for_test().publish(
                key.clone(),
                file.clone(),
                vec![diagnostic(DiagnosticSeverity::Error, "settled error")],
            );
            lsp.certify_diagnostic_content_for_test(&file);
        }
        let counts = ctx.status_bar_counts().expect("populated");
        assert_eq!((counts.errors, counts.warnings), (1, 0));

        {
            let mut lsp = ctx.lsp();
            lsp.diagnostics_store_mut_for_test()
                .publish_full_with_provisional(
                    key.clone(),
                    file.clone(),
                    vec![diagnostic(
                        DiagnosticSeverity::Warning,
                        "latest warming warning",
                    )],
                    None,
                    None,
                    true,
                );
            lsp.certify_diagnostic_content_for_test(&file);
        }
        let counts = ctx.status_bar_counts().expect("populated");
        assert_eq!(
            (counts.errors, counts.warnings),
            (1, 0),
            "pre-quiescence diagnostics must not replace authoritative counts"
        );

        {
            let mut lsp = ctx.lsp();
            assert!(lsp
                .diagnostics_store_mut_for_test()
                .promote_provisional_for_server(&key));
        }
        let counts = ctx.status_bar_counts().expect("populated");
        assert_eq!(
            (counts.errors, counts.warnings),
            (0, 1),
            "the latest report becomes authoritative at quiescence"
        );
    }

    #[test]
    fn status_bar_filtered_counts_ignore_environmental_flap() {
        use crate::lsp::diagnostics::{DiagnosticSeverity, StoredDiagnostic};
        use crate::lsp::registry::ServerKind;
        use crate::lsp::roots::ServerKey;

        let ctx = ctx();
        let fixture = tempfile::tempdir().unwrap();
        let root = crate::path_identity::canonicalize_normalized(fixture.path());
        ctx.set_canonical_cache_root(root.clone());
        ctx.update_status_bar_todos(Some(0));

        let file = root.join("aft.jsonc");
        std::fs::write(&file, "{}\n").unwrap();
        let key = ServerKey {
            kind: ServerKind::TypeScript,
            root: root.clone(),
        };
        let env = StoredDiagnostic {
            file: file.clone(),
            line: 1,
            column: 1,
            end_line: 1,
            end_column: 2,
            severity: DiagnosticSeverity::Error,
            message: "Failed to load schema from https://example.com/schema.json".into(),
            code: None,
            source: Some("json".into()),
        };

        assert_eq!(ctx.status_bar_count_values().errors, None);

        {
            let mut lsp = ctx.lsp();
            lsp.diagnostics_store_mut_for_test()
                .publish(key.clone(), file.clone(), vec![env]);
            lsp.certify_diagnostic_content_for_test(&file);
        }
        assert_eq!(
            ctx.status_bar_count_values().errors,
            Some(0),
            "an environmental-only report proves there are zero included errors"
        );

        {
            let mut lsp = ctx.lsp();
            lsp.diagnostics_store_mut_for_test()
                .publish(key, file.clone(), vec![]);
            lsp.certify_diagnostic_content_for_test(&file);
        }
        assert_eq!(
            ctx.status_bar_count_values().errors,
            Some(0),
            "clearing the excluded diagnostic keeps the proven included count at zero"
        );
    }
}

#[cfg(test)]
mod harness_path_tests {
    use super::*;
    use crate::harness::Harness;
    use crate::parser::TreeSitterProvider;

    fn ctx_with_storage_and_harness(storage_dir: PathBuf, harness: Harness) -> AppContext {
        let ctx = AppContext::new(
            Box::new(TreeSitterProvider::new()),
            Config {
                storage_dir: Some(storage_dir),
                ..Config::default()
            },
        );
        ctx.set_harness(harness);
        ctx
    }

    #[test]
    fn harness_dir_resolves_correctly() {
        let storage = PathBuf::from("/tmp/cortexkit/aft");
        let ctx = ctx_with_storage_and_harness(storage.clone(), Harness::Pi);

        assert_eq!(ctx.harness_dir(), storage.join("pi"));
    }

    #[test]
    fn bash_tasks_dir_uses_hash_session() {
        let _storage_env = crate::test_env::without_storage_override();
        let storage = PathBuf::from("/tmp/cortexkit/aft");
        let ctx = ctx_with_storage_and_harness(storage.clone(), Harness::Opencode);

        assert_eq!(
            ctx.bash_tasks_dir("ses_abc"),
            storage
                .join("opencode")
                .join("bash-tasks")
                .join(hash_session("ses_abc"))
        );
    }

    #[test]
    fn backups_dir_includes_path_hash() {
        let storage = PathBuf::from("/tmp/cortexkit/aft");
        let ctx = ctx_with_storage_and_harness(storage.clone(), Harness::Pi);

        assert_eq!(
            ctx.backups_dir("ses_abc", "pathhash"),
            storage
                .join("pi")
                .join("backups")
                .join(hash_session("ses_abc"))
                .join("pathhash")
        );
    }

    #[test]
    fn filters_dir_under_harness() {
        let storage = PathBuf::from("/tmp/cortexkit/aft");
        let ctx = ctx_with_storage_and_harness(storage.clone(), Harness::Opencode);

        assert_eq!(ctx.filters_dir(), storage.join("opencode").join("filters"));
    }

    #[test]
    fn trust_file_is_host_global() {
        let storage = PathBuf::from("/tmp/cortexkit/aft");
        let ctx = ctx_with_storage_and_harness(storage.clone(), Harness::Pi);

        assert_eq!(
            ctx.trust_file(),
            storage.join("trusted-filter-projects.json")
        );
    }

    #[test]
    fn same_session_different_harness_resolve_different_paths() {
        let storage = PathBuf::from("/tmp/cortexkit/aft");
        let opencode = ctx_with_storage_and_harness(storage.clone(), Harness::Opencode);
        let pi = ctx_with_storage_and_harness(storage, Harness::Pi);

        assert_ne!(
            opencode.bash_tasks_dir("ses_same"),
            pi.bash_tasks_dir("ses_same")
        );
    }

    #[test]
    fn callgraph_and_inspect_dirs_are_root_keyed() {
        let temp = tempfile::tempdir().expect("tempdir");
        let storage = temp.path().join("storage");
        let root = temp.path().join("checkout");
        std::fs::create_dir_all(&root).expect("create root");
        let ctx = ctx_with_storage_and_harness(storage.clone(), Harness::Opencode);
        ctx.set_canonical_cache_root(root.clone());

        assert_eq!(
            ctx.inspect_dir(),
            ctx.storage_dir()
                .join("inspect")
                .join(crate::path_identity::project_scope_key(&root))
        );
        assert!(!ctx.inspect_dir().starts_with(storage.join("opencode")));
    }

    #[test]
    fn per_domain_capability_allows_inspect_writer_when_callgraph_read_only() {
        let storage = PathBuf::from("/tmp/cortexkit/aft");
        let ctx = ctx_with_storage_and_harness(storage, Harness::Opencode);
        ctx.set_cache_writer_capabilities(false, true);

        assert!(ctx.shared_artifacts_read_only());
        assert!(!ctx.callgraph_writer());
        assert!(ctx.inspect_writer());
    }
}

#[cfg(test)]
mod shared_db_tests {
    use super::*;
    use tempfile::tempdir;

    fn bound_database_root(app: Arc<App>, root: &Path, storage: &Path) -> Arc<AppContext> {
        let ctx = Arc::new(AppContext::from_app(
            app,
            Config {
                project_root: Some(root.to_path_buf()),
                storage_dir: Some(storage.to_path_buf()),
                ..Config::default()
            },
        ));
        ctx.set_harness(crate::harness::Harness::Opencode);
        ctx.set_canonical_cache_root(root.to_path_buf());
        ctx.bash_background()
            .set_harness(crate::harness::Harness::Opencode);
        {
            let mut backup = ctx.backup().lock();
            backup.set_storage_dir(storage.to_path_buf(), 72);
            backup.set_db_harness(crate::harness::Harness::Opencode);
        }
        ctx.begin_database_runtime(
            root.to_path_buf(),
            storage.to_path_buf(),
            "idle-rebind".into(),
        );
        crate::database_open::run_staged_open(
            &ctx,
            crate::database_open::DatabaseOpenRunner::ConfigureTail,
            true,
        );
        ctx.mark_subc_bound();
        assert_eq!(ctx.database_runtime_state.load(Ordering::Acquire), 2);
        ctx
    }

    fn assert_rebound_backup_is_persisted(ctx: &AppContext, root: &Path, storage: &Path) {
        let file = root.join("edited.txt");
        std::fs::write(&file, "before rebind edit").unwrap();
        let id = ctx
            .backup()
            .lock()
            .snapshot("idle-rebind", &file, "before edit after rebind")
            .unwrap()
            .unwrap();
        std::fs::write(&file, "after rebind edit").unwrap();
        ctx.backup().lock().record_post_mutation_states();
        let conn = ctx.db().unwrap();
        let count: u32 = conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM backups WHERE backup_id = ?1 AND harness = 'opencode'
             AND session_id = 'idle-rebind' AND project_key = ?2",
                rusqlite::params![id, crate::path_identity::project_scope_key(root)],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            count, 1,
            "backup taken after rebind must be mirrored to aft.db"
        );

        // A fresh store must recover the persisted backup rather than reuse
        // the original store's in-memory backup entries.
        let mut recovered = BackupStore::new();
        recovered.set_storage_dir(storage.to_path_buf(), 72);
        recovered.set_db_harness(crate::harness::Harness::Opencode);
        recovered.set_db_project_key(crate::path_identity::project_scope_key(root));
        recovered.set_db_pool(conn);
        let (entry, warning) = recovered.restore_latest("idle-rebind", &file).unwrap();
        assert_eq!(entry.backup_id, id);
        assert!(warning.is_none(), "unexpected restore warning: {warning:?}");
        assert_eq!(std::fs::read_to_string(file).unwrap(), "before rebind edit");
    }

    #[cfg(unix)]
    fn completed_rebind_bash_task(ctx: &AppContext, root: &Path, storage: &Path) -> String {
        let id = ctx
            .bash_background()
            .spawn(
                crate::sandbox_spawn::SpawnPlan::Unsandboxed,
                "printf 'idle rebind persistence\\n'",
                "idle-rebind".into(),
                root.to_path_buf(),
                std::collections::HashMap::new(),
                crate::bash_background::HardKill::After(Duration::from_secs(30)),
                None,
                storage.to_path_buf(),
                16,
                true,
                true,
                Some(root.to_path_buf()),
            )
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(completion) = ctx
                .bash_background()
                .drain_completions_for_session(Some("idle-rebind"))
                .into_iter()
                .find(|completion| completion.task_id == id)
            {
                assert_eq!(completion.exit_code, Some(0));
                return id;
            }
            assert!(Instant::now() < deadline, "bash task {id} did not complete");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(unix)]
    fn assert_rebind_bash_row(ctx: &AppContext, id: &str) {
        let count: u32 = ctx
            .db()
            .unwrap()
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM bash_tasks WHERE task_id = ?1 AND status = 'completed'",
                [id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "bash task {id} must have an aft.db row");
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let events: u32 = ctx
                .db()
                .unwrap()
                .lock()
                .unwrap()
                .query_row(
                    "SELECT COUNT(*) FROM compression_events WHERE task_id = ?1",
                    [id],
                    |row| row.get(0),
                )
                .unwrap();
            if events == 1 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "bash task {id} lost its compression event"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(unix)]
    #[test]
    fn ready_root_rebind_reattaches_pools_after_idle_release() {
        let storage = tempdir().unwrap();
        let root_a = tempdir().unwrap();
        let root_b = tempdir().unwrap();
        let app = App::default_shared();
        let a = bound_database_root(app.clone(), root_a.path(), storage.path());
        let b = bound_database_root(app.clone(), root_b.path(), storage.path());
        let shared = b.db().unwrap();
        assert!(Arc::ptr_eq(&a.db().unwrap(), &shared));
        let first = completed_rebind_bash_task(&a, root_a.path(), storage.path());
        assert_rebind_bash_row(&a, &first);

        a.mark_subc_unbound();
        a.release_idle_reopenable_resources_in_background()
            .join()
            .unwrap();
        assert!(!a.bash_background().uses_db_pool(&shared));
        assert!(!a.backup().lock().uses_db_pool(&shared));
        assert!(!b.subc_unbound_quiesced());
        assert!(b.bash_background().uses_db_pool(&shared));
        assert!(b.backup().lock().uses_db_pool(&shared));
        assert!(Arc::ptr_eq(
            &app.db_for_path(&storage.path().join("aft.db")).unwrap(),
            &shared
        ));
        a.begin_database_runtime(
            root_a.path().to_path_buf(),
            storage.path().to_path_buf(),
            "idle-rebind".into(),
        );
        a.mark_subc_bound();
        assert_eq!(a.database_open_attempts.load(Ordering::Acquire), 1);
        assert!(!a.database_runtime_initializing());
        let second = completed_rebind_bash_task(&a, root_a.path(), storage.path());
        assert_ne!(first, second);
        assert_rebind_bash_row(&a, &second);
        assert_rebound_backup_is_persisted(&a, root_a.path(), storage.path());
    }

    #[test]
    fn ready_root_reconfigure_repairs_each_detached_pool_without_another_open() {
        let storage = tempdir().unwrap();
        let root = tempdir().unwrap();
        let ctx = bound_database_root(App::default_shared(), root.path(), storage.path());
        let shared = ctx.db().unwrap();
        ctx.backup().lock().clear_db_pool();
        ctx.begin_database_runtime(
            root.path().to_path_buf(),
            storage.path().to_path_buf(),
            "idle-rebind".into(),
        );
        assert!(ctx.backup().lock().uses_db_pool(&shared));
        assert!(ctx.bash_background().uses_db_pool(&shared));
        ctx.bash_background().clear_db_pool();
        ctx.begin_database_runtime(
            root.path().to_path_buf(),
            storage.path().to_path_buf(),
            "idle-rebind".into(),
        );
        assert!(ctx.backup().lock().uses_db_pool(&shared));
        assert!(ctx.bash_background().uses_db_pool(&shared));
        assert!(!ctx.database_runtime_initializing());
        assert_eq!(ctx.database_open_attempts.load(Ordering::Acquire), 1);
    }

    #[test]
    fn ready_root_rebind_orders_pool_attach_after_release_and_before_late_release() {
        let storage = tempdir().unwrap();
        let root = tempdir().unwrap();
        let ctx = bound_database_root(App::default_shared(), root.path(), storage.path());
        ctx.mark_subc_unbound();

        // Configure commits before the bound transition. Force idle release into
        // that gap, so repairing pools only during configure cannot pass.
        ctx.begin_database_runtime(
            root.path().to_path_buf(),
            storage.path().to_path_buf(),
            "idle-rebind".into(),
        );
        ctx.release_idle_reopenable_resources_in_background()
            .join()
            .unwrap();
        ctx.mark_subc_bound();
        assert_rebound_backup_is_persisted(&ctx, root.path(), storage.path());

        // After the bind, an idle release must not clear the database pools.
        ctx.release_idle_reopenable_resources_in_background()
            .join()
            .unwrap();
        assert_rebound_backup_is_persisted(&ctx, root.path(), storage.path());
        assert_eq!(ctx.database_open_attempts.load(Ordering::Acquire), 1);
    }

    fn failed_database_context() -> (tempfile::TempDir, tempfile::TempDir, AppContext) {
        // Each context owns its storage fixture. A process-wide override would
        // redirect unrelated contexts while libtest runs them in parallel.
        let storage = tempdir().unwrap();
        let root = tempdir().unwrap();
        let path = storage.path().join("aft.db");
        let conn = crate::db::open(&path).unwrap();
        conn.execute(
            "UPDATE schema_version SET version = ?1",
            [crate::db::CURRENT_SCHEMA_VERSION + 1],
        )
        .unwrap();
        drop(conn);
        let ctx = AppContext::from_app(
            App::default_shared(),
            Config {
                project_root: Some(root.path().to_path_buf()),
                storage_dir: Some(storage.path().to_path_buf()),
                ..Config::default()
            },
        );
        ctx.set_canonical_cache_root(root.path().to_path_buf());
        ctx.begin_database_runtime(
            root.path().to_path_buf(),
            storage.path().to_path_buf(),
            "database-recovery".into(),
        );
        crate::database_open::run_staged_open(
            &ctx,
            crate::database_open::DatabaseOpenRunner::ConfigureTail,
            true,
        );
        assert_eq!(ctx.database_runtime_state.load(Ordering::Acquire), 3);
        assert_eq!(ctx.database_open_attempts.load(Ordering::Acquire), 1);
        let response = ctx.database_runtime_refusal("failed", "bash").unwrap();
        assert!(serde_json::to_value(response).unwrap()["message"]
            .as_str()
            .unwrap()
            .contains("storage_requires_newer_reader"));
        assert!(ctx.db().is_none());
        (storage, root, ctx)
    }

    #[test]
    fn database_fixtures_keep_storage_local_to_each_context() {
        let (first_storage, _first_root, first) = failed_database_context();
        let (second_storage, _second_root, second) = failed_database_context();
        assert_eq!(second.storage_dir(), second_storage.path());
        assert_eq!(first.storage_dir(), first_storage.path());
    }

    fn persistence_call(ctx: &AppContext) -> Option<crate::protocol::Response> {
        ctx.database_runtime_refusal("retry", "db_set_host_state")
    }

    #[test]
    fn failed_database_open_recovers_after_backoff_without_rebind() {
        let (storage, _root, ctx) = failed_database_context();
        // The failed open held no connection. Repair only this test's database,
        // closing the repair connection before AFT attempts to reopen it.
        let conn = rusqlite::Connection::open(storage.path().join("aft.db")).unwrap();
        conn.execute(
            "UPDATE schema_version SET version = ?1",
            [crate::db::CURRENT_SCHEMA_VERSION],
        )
        .unwrap();
        drop(conn);
        std::thread::sleep(Duration::from_millis(2_100));

        let refusal = persistence_call(&ctx);
        assert!(
            refusal.is_none(),
            "repaired database stayed latched: {refusal:?}"
        );
        assert_eq!(ctx.database_open_attempts.load(Ordering::Acquire), 2);
        assert!(ctx.db().is_some());
        let failure = ctx.database_runtime_failure.lock();
        assert!(failure.error.is_none());
        assert!(failure.retry_at.is_none());
        assert_eq!(failure.backoff, Duration::ZERO);
        drop(failure);
        let request: crate::protocol::RawRequest = serde_json::from_value(serde_json::json!({
            "id": "persist", "command": "db_set_host_state",
            "params": {"key": "recovered", "value": "without-rebind"}
        }))
        .unwrap();
        assert!(crate::commands::state::handle_db_set_host_state(&request, &ctx).success);
        let db = ctx.db().unwrap();
        let conn = db.lock().unwrap();
        assert_eq!(
            crate::db::state::get_host_state(&conn, "recovered").unwrap(),
            Some("without-rebind".into())
        );
    }

    #[test]
    fn failed_database_open_does_not_reopen_inside_backoff() {
        let (_storage, _root, ctx) = failed_database_context();
        for _ in 0..10 {
            assert!(persistence_call(&ctx).is_some());
        }
        assert_eq!(ctx.database_open_attempts.load(Ordering::Acquire), 1);
        assert!(ctx.database_runtime_refusal("read", "read").is_none());
    }

    fn make_database_retry_due(ctx: &AppContext) {
        ctx.database_runtime_failure.lock().retry_at =
            Some(Instant::now() - Duration::from_secs(1));
    }

    fn failed_floor_context() -> (tempfile::TempDir, tempfile::TempDir, AppContext) {
        let storage = tempdir().unwrap();
        let root = tempdir().unwrap();
        std::fs::write(
            storage.path().join(crate::reader_floor::FLOOR_FILE),
            serde_json::to_vec(&serde_json::json!({
                "floor_schema": crate::reader_floor::FLOOR_SCHEMA,
                "stores": { "aft.db": crate::db::CURRENT_SCHEMA_VERSION + 1 }
            }))
            .unwrap(),
        )
        .unwrap();
        crate::reader_floor::prepare(storage.path());
        let ctx = AppContext::from_app(
            App::default_shared(),
            Config {
                project_root: Some(root.path().to_path_buf()),
                storage_dir: Some(storage.path().to_path_buf()),
                ..Config::default()
            },
        );
        ctx.set_canonical_cache_root(root.path().to_path_buf());
        ctx.begin_database_runtime(
            root.path().to_path_buf(),
            storage.path().to_path_buf(),
            "floor-recovery".into(),
        );
        crate::database_open::run_staged_open(
            &ctx,
            crate::database_open::DatabaseOpenRunner::ConfigureTail,
            true,
        );
        let refusal = serde_json::to_value(persistence_call(&ctx).unwrap()).unwrap();
        assert!(refusal["message"]
            .as_str()
            .unwrap()
            .contains("reader floor"));
        assert!(!storage.path().join("aft.db").exists());
        (storage, root, ctx)
    }

    #[test]
    fn reader_floor_retry_recovers_after_operator_lowers_floor_without_restart() {
        let (storage, _root, ctx) = failed_floor_context();
        std::fs::write(
            storage.path().join(crate::reader_floor::FLOOR_FILE),
            serde_json::to_vec(&serde_json::json!({
                "floor_schema": crate::reader_floor::FLOOR_SCHEMA,
                "stores": { "aft.db": crate::db::CURRENT_SCHEMA_VERSION }
            }))
            .unwrap(),
        )
        .unwrap();
        assert!(persistence_call(&ctx).is_some(), "backoff must still apply");
        assert_eq!(ctx.database_open_attempts.load(Ordering::Acquire), 1);
        assert_eq!(
            crate::persisted_format::refusal_covering(
                crate::persisted_format::PersistedStore::AftDb,
                &storage.path().join("aft.db")
            )
            .unwrap()
            .found,
            u64::from(crate::db::CURRENT_SCHEMA_VERSION + 1),
            "the floor must not be refreshed before the retry backoff expires"
        );
        make_database_retry_due(&ctx);
        assert!(
            persistence_call(&ctx).is_none(),
            "operator-lowered reader floor remained latched after the retry backoff"
        );
        let request = serde_json::from_value(serde_json::json!({
            "id": "floor-recovered", "command": "db_set_host_state",
            "params": { "key": "floor-recovered", "value": "yes" }
        }))
        .unwrap();
        let response = crate::commands::state::handle_db_set_host_state(&request, &ctx);
        assert!(response.success, "{response:?}");
        let db = ctx.db().unwrap();
        assert_eq!(
            crate::db::state::get_host_state(&db.lock().unwrap(), "floor-recovered").unwrap(),
            Some("yes".into())
        );
    }

    #[test]
    fn reader_floor_retry_refuses_unparseable_floor_written_mid_run() {
        let (storage, _root, ctx) = failed_floor_context();
        let path = storage.path().join(crate::reader_floor::FLOOR_FILE);
        std::fs::write(&path, b"not a reader floor").unwrap();
        for _ in 0..2 {
            make_database_retry_due(&ctx);
            let refusal = serde_json::to_value(persistence_call(&ctx).unwrap()).unwrap();
            assert!(
                refusal["message"]
                    .as_str()
                    .unwrap()
                    .contains("cannot be parsed"),
                "{refusal}"
            );
            assert_eq!(std::fs::read(&path).unwrap(), b"not a reader floor");
            assert!(!storage.path().join("aft.db").exists());
            assert_eq!(
                crate::persisted_format::refusals_under(storage.path()).len(),
                crate::persisted_format::PersistedStore::ALL.len()
            );
        }
    }

    fn resident_database_context() -> (tempfile::TempDir, tempfile::TempDir, AppContext) {
        let storage = tempdir().unwrap();
        let root = tempdir().unwrap();
        let ctx = AppContext::from_app(
            App::default_shared(),
            Config {
                project_root: Some(root.path().to_path_buf()),
                storage_dir: Some(storage.path().to_path_buf()),
                ..Config::default()
            },
        );
        ctx.set_harness(crate::harness::Harness::Opencode);
        ctx.set_canonical_cache_root(root.path().to_path_buf());
        ctx.begin_database_runtime(
            root.path().to_path_buf(),
            storage.path().to_path_buf(),
            "resident-recovery".into(),
        );
        crate::database_open::run_staged_open(
            &ctx,
            crate::database_open::DatabaseOpenRunner::ConfigureTail,
            true,
        );
        assert_eq!(ctx.database_runtime_state.load(Ordering::Acquire), 2);
        (storage, root, ctx)
    }

    // A separate process, not a second fd in this process, changes the live
    // fixture while the App retains its SQLite handle and WAL locks.
    fn change_schema_in_child(path: &Path, version: u32) {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "context::shared_db_tests::repaired_resident_database_rechecks_schema_and_pushes_recovered_status",
                "--nocapture",
            ])
            .env("AFT_TEST_SCHEMA_REPAIR_PATH", path)
            .env("AFT_TEST_SCHEMA_REPAIR_VERSION", version.to_string())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "schema repair child failed: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
    }

    fn assert_database_status(ctx: &AppContext, refused: bool) {
        let status = ctx.build_status_snapshot();
        let reasons = status["degraded_reasons"].as_array().unwrap();
        assert_eq!(
            reasons
                .iter()
                .any(|reason| reason == "storage_requires_newer_reader:aft.db"),
            refused,
            "unexpected database degraded reasons: {reasons:?}"
        );
        assert_eq!(
            status["storage_refusals"].as_array().unwrap().len(),
            usize::from(refused)
        );
    }

    #[test]
    fn repaired_resident_database_rechecks_schema_and_pushes_recovered_status() {
        if let Some(path) = std::env::var_os("AFT_TEST_SCHEMA_REPAIR_PATH") {
            crate::test_storage::assert_database(Path::new(&path));
            let version: u32 = std::env::var("AFT_TEST_SCHEMA_REPAIR_VERSION")
                .unwrap()
                .parse()
                .unwrap();
            let conn = rusqlite::Connection::open(path).unwrap();
            conn.execute_batch("BEGIN IMMEDIATE").unwrap();
            conn.execute("UPDATE schema_version SET version = ?1", [version])
                .unwrap();
            conn.execute_batch("COMMIT").unwrap();
            return;
        }
        let (storage, _root, owner) = resident_database_context();
        let resident = owner.db().unwrap();
        let path = storage.path().join("aft.db");
        change_schema_in_child(&path, crate::db::CURRENT_SCHEMA_VERSION + 1);
        let affected_root = tempdir().unwrap();
        let ctx = Arc::new(AppContext::from_app(
            owner.app(),
            Config {
                project_root: Some(affected_root.path().to_path_buf()),
                storage_dir: Some(storage.path().to_path_buf()),
                ..Config::default()
            },
        ));
        ctx.set_harness(crate::harness::Harness::Opencode);
        ctx.set_degraded_reasons(vec!["artifact_owner_read_only".into()]);
        ctx.set_canonical_cache_root(affected_root.path().to_path_buf());
        ctx.begin_database_runtime(
            affected_root.path().to_path_buf(),
            storage.path().to_path_buf(),
            "affected-route".into(),
        );
        let peeks = crate::db::schema_peek_count_for_test();
        crate::database_open::run_staged_open(
            &ctx,
            crate::database_open::DatabaseOpenRunner::ConfigureTail,
            true,
        );
        assert!(persistence_call(&ctx).is_some());
        assert_database_status(&ctx, true);

        change_schema_in_child(&path, crate::db::CURRENT_SCHEMA_VERSION);
        // Expire the real two-second backoff deterministically without sleeping.
        assert_eq!(
            ctx.database_runtime_failure.lock().backoff,
            Duration::from_secs(2)
        );
        make_database_retry_due(&ctx);
        let (tx, rx) = std::sync::mpsc::channel();
        ctx.bind_status_context();
        ctx.set_progress_sender(Some(Arc::new(Box::new(move |frame| {
            let _ = tx.send(frame);
        }))));
        assert!(
            persistence_call(&ctx).is_none(),
            "repaired schema remained refused"
        );
        assert_database_status(&ctx, false);
        assert!(Arc::ptr_eq(&resident, &ctx.db().unwrap()));
        assert_eq!(
            crate::db::schema_peek_count_for_test(),
            peeks,
            "recovery opened a second descriptor"
        );
        let request: crate::protocol::RawRequest = serde_json::from_value(serde_json::json!({
            "id": "persist", "command": "db_set_host_state",
            "params": {"key": "recovered", "value": "resident"}
        }))
        .unwrap();
        assert!(crate::commands::state::handle_db_set_host_state(&request, &ctx).success);
        assert_eq!(
            crate::db::state::get_host_state(&resident.lock().unwrap(), "recovered").unwrap(),
            Some("resident".into())
        );
        let frame = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("recovery must push status_changed without another index event");
        let crate::protocol::PushFrame::StatusChanged(status) = frame else {
            panic!("unexpected frame: {frame:?}")
        };
        assert!(!status.snapshot["degraded_reasons"]
            .as_array()
            .unwrap()
            .iter()
            .any(|reason| reason == "storage_requires_newer_reader:aft.db"));
        assert!(status.snapshot["degraded_reasons"]
            .as_array()
            .unwrap()
            .iter()
            .any(|reason| reason == "artifact_owner_read_only"));
    }

    #[test]
    fn resident_database_busy_retry_preserves_refusal_until_schema_is_rechecked() {
        let (storage, _root, ctx) = resident_database_context();
        let resident = ctx.db().unwrap();
        let path = storage.path().join("aft.db");
        change_schema_in_child(&path, crate::db::CURRENT_SCHEMA_VERSION + 1);
        ctx.finish_database_runtime_error("recheck schema".into(), false);
        make_database_retry_due(&ctx);
        assert!(persistence_call(&ctx).is_some());
        assert_database_status(&ctx, true);
        let peeks = crate::db::schema_peek_count_for_test();
        let held = resident.lock().unwrap();
        make_database_retry_due(&ctx);
        let response = serde_json::to_value(persistence_call(&ctx).unwrap()).unwrap();
        assert!(response["message"]
            .as_str()
            .unwrap()
            .contains("process-shared database connection is in use"));
        drop(held);
        assert_database_status(&ctx, true);
        change_schema_in_child(&path, crate::db::CURRENT_SCHEMA_VERSION);
        assert!(
            persistence_call(&ctx).is_none(),
            "busy refusal stayed latched after releasing the connection"
        );
        assert_database_status(&ctx, false);
        assert!(Arc::ptr_eq(&resident, &ctx.db().unwrap()));
        assert_eq!(crate::db::schema_peek_count_for_test(), peeks);
    }

    #[test]
    fn busy_database_retry_recovers_from_brief_database_slot_contention() {
        let storage = tempdir().unwrap();
        let root = tempdir().unwrap();
        let ctx = Arc::new(AppContext::from_app(
            App::default_shared(),
            Config {
                project_root: Some(root.path().to_path_buf()),
                storage_dir: Some(storage.path().to_path_buf()),
                ..Config::default()
            },
        ));
        ctx.set_harness(crate::harness::Harness::Opencode);
        ctx.set_canonical_cache_root(root.path().to_path_buf());
        // Recreate the reported busy state after deferred initialization exhausted
        // its budget. SQLite is available now, but another reader owns the App slot.
        ctx.finish_database_runtime_error(
            "sqlite PRAGMA journal_mode=WAL: database is locked".into(),
            true,
        );
        let exhausted = serde_json::to_value(persistence_call(&ctx).unwrap()).unwrap();
        assert_eq!(exhausted["code"], "database_unavailable", "{exhausted}");
        assert_eq!(ctx.database_runtime_state.load(Ordering::Acquire), 5);

        let app = ctx.app();
        let held = app.db.lock();
        let (contended_tx, contended_rx) = std::sync::mpsc::channel();
        let (released_tx, released_rx) = std::sync::mpsc::channel();
        let retry_ctx = Arc::clone(&ctx);
        let retry = std::thread::spawn(move || {
            DATABASE_SLOT_CONTENDED_FOR_TEST.with(|hook| {
                *hook.borrow_mut() = Some(Box::new(move || {
                    contended_tx.send(()).unwrap();
                    released_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                }));
            });
            persistence_call(&retry_ctx)
        });
        contended_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        drop(held);
        released_tx.send(()).unwrap();
        let refusal = retry
            .join()
            .unwrap()
            .map(|response| serde_json::to_value(response).unwrap());
        eprintln!(
            "write after SQLite became available, with transient App slot contention: {refusal:?}"
        );
        assert!(
            refusal.is_none(),
            "available database was refused: {refusal:?}"
        );
        let request: crate::protocol::RawRequest = serde_json::from_value(serde_json::json!({
            "id": "released", "command": "db_set_host_state",
            "params": { "key": "released", "value": "yes" }
        }))
        .unwrap();
        let response = crate::commands::state::handle_db_set_host_state(&request, &ctx);
        assert!(response.success, "{response:?}");
        let db = ctx.db().unwrap();
        assert_eq!(
            crate::db::state::get_host_state(&db.lock().unwrap(), "released").unwrap(),
            Some("yes".into())
        );
    }

    #[test]
    fn resident_database_open_slot_busy_recovers_on_next_call() {
        let (_storage, _root, ctx) = resident_database_context();
        let resident = ctx.db().unwrap();
        let peeks = crate::db::schema_peek_count_for_test();
        ctx.finish_database_runtime_error("retry open".into(), false);
        make_database_retry_due(&ctx);
        let app = ctx.app();
        let held = app.db.lock();
        let response = serde_json::to_value(persistence_call(&ctx).unwrap()).unwrap();
        assert!(response["message"]
            .as_str()
            .unwrap()
            .contains("process-shared database connection is in use"));
        assert_eq!(ctx.database_runtime_state.load(Ordering::Acquire), 5);
        drop(held);
        assert!(
            persistence_call(&ctx).is_none(),
            "busy refusal stayed latched after releasing the open slot"
        );
        assert!(Arc::ptr_eq(&resident, &ctx.db().unwrap()));
        assert_eq!(crate::db::schema_peek_count_for_test(), peeks);
    }

    #[test]
    fn permanent_database_error_is_rechecked_with_doubling_capped_backoff() {
        let (storage, _root, ctx) = failed_database_context();
        assert_eq!(
            ctx.database_runtime_failure.lock().backoff,
            Duration::from_secs(2)
        );
        // A different unsupported version must appear in the next refusal,
        // proving that retries re-check the file rather than replaying an error.
        let conn = rusqlite::Connection::open(storage.path().join("aft.db")).unwrap();
        conn.execute(
            "UPDATE schema_version SET version = ?1",
            [crate::db::CURRENT_SCHEMA_VERSION + 2],
        )
        .unwrap();
        drop(conn);
        for (index, seconds) in [4, 8, 16, 32, 60, 60].into_iter().enumerate() {
            make_database_retry_due(&ctx);
            assert!(ctx.database_runtime_refusal("read", "read").is_none());
            assert!(!ctx.claim_database_runtime_retry("read"));
            let response = serde_json::to_value(persistence_call(&ctx).unwrap()).unwrap();
            let message = response["message"].as_str().unwrap();
            assert!(message.contains("storage_requires_newer_reader"));
            assert!(message.contains(&format!(
                "format version {}",
                crate::db::CURRENT_SCHEMA_VERSION + 2
            )));
            assert!(message.contains("next persistence-requiring call after"));
            assert!(message.contains("AFT retries"));
            assert!(!message.contains("rebind to retry"));
            assert_eq!(response["retryable"], true);
            assert_eq!(
                ctx.database_runtime_failure.lock().backoff,
                Duration::from_secs(seconds)
            );
            assert_eq!(
                ctx.database_open_attempts.load(Ordering::Acquire),
                index as u64 + 2
            );
            assert!(persistence_call(&ctx).is_some());
            assert_eq!(
                ctx.database_open_attempts.load(Ordering::Acquire),
                index as u64 + 2
            );
            assert!(ctx.db().is_none());
        }
    }

    #[test]
    fn only_one_concurrent_call_claims_a_failed_database_retry() {
        let (_storage, _root, ctx) = failed_database_context();
        let ctx = Arc::new(ctx);
        make_database_retry_due(&ctx);
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let calls = (0..8)
            .map(|_| {
                let ctx = Arc::clone(&ctx);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    ctx.claim_database_runtime_retry("bash")
                })
            })
            .collect::<Vec<_>>();
        assert_eq!(
            calls
                .into_iter()
                .map(|call| call.join().unwrap())
                .filter(|claimed| *claimed)
                .count(),
            1
        );
        ctx.retry_database_runtime();
        assert_eq!(ctx.database_open_attempts.load(Ordering::Acquire), 2);
    }

    #[test]
    fn failed_database_retry_reuses_and_preserves_a_resident_connection() {
        let storage = tempdir().unwrap();
        let root = tempdir().unwrap();
        let ctx = AppContext::from_app(
            App::default_shared(),
            Config {
                project_root: Some(root.path().to_path_buf()),
                storage_dir: Some(storage.path().to_path_buf()),
                ..Config::default()
            },
        );
        ctx.set_canonical_cache_root(root.path().to_path_buf());
        let path = storage.path().join("aft.db");
        let resident = ctx.app().open_db(&path).unwrap();
        let peeks = crate::db::schema_peek_count_for_test();
        resident
            .lock()
            .unwrap()
            .execute(
                "UPDATE schema_version SET version = ?1",
                [crate::db::CURRENT_SCHEMA_VERSION + 1],
            )
            .unwrap();
        ctx.begin_database_runtime(
            root.path().to_path_buf(),
            storage.path().to_path_buf(),
            "resident".into(),
        );
        crate::database_open::run_staged_open(
            &ctx,
            crate::database_open::DatabaseOpenRunner::ConfigureTail,
            true,
        );
        assert_eq!(
            crate::db::schema_peek_count_for_test(),
            peeks,
            "resident schema checks must not open another descriptor"
        );
        let response = serde_json::to_value(persistence_call(&ctx).unwrap()).unwrap();
        assert!(response["message"]
            .as_str()
            .unwrap()
            .contains("storage_requires_newer_reader"));
        assert!(Arc::ptr_eq(
            &resident,
            &ctx.db().expect("refusal must preserve the shared handle")
        ));
        resident
            .lock()
            .unwrap()
            .execute(
                "UPDATE schema_version SET version = ?1",
                [crate::db::CURRENT_SCHEMA_VERSION],
            )
            .unwrap();
        make_database_retry_due(&ctx);
        assert!(persistence_call(&ctx).is_none());
        assert_eq!(
            crate::db::schema_peek_count_for_test(),
            peeks,
            "retries must not open another descriptor on a resident database"
        );
        assert!(Arc::ptr_eq(&resident, &ctx.db().unwrap()));
        assert_eq!(ctx.database_open_attempts.load(Ordering::Acquire), 2);
    }

    #[test]
    fn database_single_attempt_bounds_wait_for_a_held_connection_or_open_slot() {
        let storage = tempdir().unwrap();
        let path = storage.path().join("aft.db");
        let app = App::default_shared();
        let resident = app.open_db(&path).unwrap();
        let held = resident.lock().unwrap();
        assert!(app
            .database_schema_version(&path, crate::db::OpenMode::SingleAttempt)
            .unwrap_err()
            .is_busy());
        drop(held);
        let held = app.db.lock();
        let started = Instant::now();
        assert!(app
            .database_schema_version(&path, crate::db::OpenMode::SingleAttempt)
            .unwrap_err()
            .is_busy());
        assert!(app
            .open_db_with_mode(&path, crate::db::OpenMode::SingleAttempt)
            .unwrap_err()
            .is_busy());
        assert!(started.elapsed() < Duration::from_secs(2));
        drop(held);
    }

    #[test]
    fn database_retry_without_configured_root_refuses_without_panicking() {
        let ctx = AppContext::from_app(App::default_shared(), Config::default());
        assert!(ctx.canonical_cache_root_opt().is_none());
        ctx.finish_database_runtime_error("database busy".into(), true);
        assert!(ctx
            .database_runtime_refusal("first", "db_set_host_state")
            .is_some());
        assert!(ctx.claim_database_runtime_retry("db_set_host_state"));
        ctx.retry_database_runtime();
        let response = serde_json::to_value(
            ctx.database_runtime_refusal("retry", "db_set_host_state")
                .unwrap(),
        )
        .unwrap();
        assert_eq!(response["code"], "database_unavailable");
        assert_eq!(response["retryable"], true);
        assert!(response["message"]
            .as_str()
            .unwrap()
            .contains("configured database root is missing"));
        assert!(!ctx.claim_database_runtime_retry("db_set_host_state"));
        assert!(ctx.database_runtime_refusal("read", "read").is_none());
    }

    #[test]
    fn app_contexts_share_one_database_connection() {
        let storage = tempdir().expect("storage tempdir");
        let root_one = tempdir().expect("first root tempdir");
        let root_two = tempdir().expect("second root tempdir");
        let app = App::default_shared();
        let ctx_one = AppContext::from_app(
            Arc::clone(&app),
            Config {
                project_root: Some(root_one.path().to_path_buf()),
                ..Config::default()
            },
        );
        let ctx_two = AppContext::from_app(
            Arc::clone(&app),
            Config {
                project_root: Some(root_two.path().to_path_buf()),
                ..Config::default()
            },
        );
        let path = storage.path().join("aft.db");

        let first = app.open_db(&path).expect("open shared database");
        let second = app.open_db(&path).expect("reuse shared database");

        assert!(Arc::ptr_eq(&first, &second));
        assert!(Arc::ptr_eq(
            &ctx_one.db().expect("first context database"),
            &ctx_two.db().expect("second context database")
        ));
    }

    /// On a fresh machine the startup catalog runs before `aft.db` exists, so
    /// creating the database must declare it without waiting for a restart.
    #[test]
    fn creating_the_database_declares_it_in_the_engram_catalog() {
        let parent = tempdir().expect("storage parent");
        let storage = parent.path().join("aft");
        std::fs::create_dir(&storage).unwrap();
        crate::engram_catalog::refresh_catalog(&storage);
        let catalog = storage.join("engram-catalog.json");
        let declares_db = || {
            let value: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&catalog).unwrap()).unwrap();
            value["entries"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["entry_id"] == "aft/db")
        };
        assert!(!declares_db(), "no database yet, so no aft/db entry");

        let app = App::default_shared();
        app.open_db(&storage.join("aft.db"))
            .expect("create database");

        assert!(declares_db(), "the new database is declared at once");
    }
}

#[cfg(test)]
mod gitignore_tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use tempfile::TempDir;

    fn make_ctx_with_root(root: &Path) -> AppContext {
        let provider = Box::new(crate::parser::TreeSitterProvider::new());
        let config = Config {
            project_root: Some(root.to_path_buf()),
            ..Config::default()
        };
        AppContext::new(provider, config)
    }

    /// Helper: returns true when the matcher would skip `path` (as if it
    /// arrived via a watcher event for this project root). Canonicalizes
    /// the query path so symlink prefixes (e.g. macOS `/var` → `/private/var`)
    /// don't trip the `ignore` crate's "path is expected to be under the
    /// root" panic — production code does the same guard via
    /// `path.starts_with(matcher.path())` in `drain_watcher_events`.
    fn is_ignored(ctx: &AppContext, path: &Path) -> bool {
        let Some(matcher) = ctx.gitignore() else {
            return false;
        };
        let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        if !canonical.starts_with(matcher.path()) {
            return false;
        }
        let is_dir = canonical.is_dir();
        matcher
            .matched_path_or_any_parents(&canonical, is_dir)
            .is_ignore()
    }

    /// Run `f` with global git-ignore discovery neutralized.
    ///
    /// `rebuild_gitignore` loads git's global excludes via the `ignore`
    /// crate, which discovers them from TWO places: `core.excludesfile` in
    /// `$HOME/.gitconfig` (or `$XDG_CONFIG_HOME/git/config`), and the default
    /// `$XDG_CONFIG_HOME/git/ignore` / `$HOME/.config/git/ignore` locations.
    /// A developer machine commonly has one of these, so a "no project ignore
    /// → None" assertion is only deterministic when BOTH discovery roots point
    /// at an empty directory — neutralizing only `XDG_CONFIG_HOME` still finds
    /// a `~/.gitconfig` `core.excludesfile`. Serialized on the process-wide
    /// env lock shared with every other HOME-mutating test; env is restored
    /// before the closure result is used.
    fn with_neutralized_global_gitignore<R>(f: impl FnOnce() -> R) -> R {
        let _guard = crate::test_env::process_env_lock();
        let tmp = TempDir::new().unwrap();
        let prev_xdg = std::env::var_os("XDG_CONFIG_HOME");
        let prev_home = std::env::var_os("HOME");
        let prev_userprofile = std::env::var_os("USERPROFILE");
        // SAFETY: serialized by the process env lock; restored immediately
        // after `f`.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", tmp.path());
            std::env::set_var("HOME", tmp.path());
            std::env::set_var("USERPROFILE", tmp.path());
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        unsafe {
            match prev_xdg {
                Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
            match prev_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
            match prev_userprofile {
                Some(v) => std::env::set_var("USERPROFILE", v),
                None => std::env::remove_var("USERPROFILE"),
            }
        }
        match result {
            Ok(r) => r,
            Err(p) => std::panic::resume_unwind(p),
        }
    }

    #[test]
    fn rebuild_gitignore_returns_none_without_project_root() {
        let provider = Box::new(crate::parser::TreeSitterProvider::new());
        let ctx = AppContext::new(provider, Config::default());
        with_neutralized_global_gitignore(|| ctx.rebuild_gitignore());
        assert!(ctx.gitignore().is_none());
    }

    #[test]
    fn rebuild_gitignore_returns_none_for_project_with_no_gitignore() {
        let tmp = TempDir::new().unwrap();
        let ctx = make_ctx_with_root(tmp.path());
        with_neutralized_global_gitignore(|| ctx.rebuild_gitignore());
        assert!(ctx.gitignore().is_none());
    }

    #[test]
    fn matcher_filters_files_in_ignored_dist_dir() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join(".gitignore"), "dist/\nbuild/\n").unwrap();
        fs::create_dir_all(tmp.path().join("dist")).unwrap();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        let dist_file = tmp.path().join("dist").join("bundle.js");
        let src_file = tmp.path().join("src").join("app.ts");
        fs::write(&dist_file, "x").unwrap();
        fs::write(&src_file, "y").unwrap();

        let ctx = make_ctx_with_root(tmp.path());
        ctx.rebuild_gitignore();

        assert!(ctx.gitignore().is_some());
        assert!(
            is_ignored(&ctx, &dist_file),
            "dist/bundle.js should be ignored"
        );
        assert!(
            !is_ignored(&ctx, &src_file),
            "src/app.ts should NOT be ignored"
        );
    }

    #[test]
    fn matcher_handles_node_modules_and_target() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join(".gitignore"), "node_modules/\ntarget/\n").unwrap();
        fs::create_dir_all(tmp.path().join("node_modules/foo")).unwrap();
        fs::create_dir_all(tmp.path().join("target/debug")).unwrap();
        let nm_file = tmp.path().join("node_modules/foo/index.js");
        let target_file = tmp.path().join("target/debug/aft");
        fs::write(&nm_file, "x").unwrap();
        fs::write(&target_file, "x").unwrap();

        let ctx = make_ctx_with_root(tmp.path());
        ctx.rebuild_gitignore();

        assert!(is_ignored(&ctx, &nm_file));
        assert!(is_ignored(&ctx, &target_file));
    }

    #[test]
    fn matcher_honors_negation_pattern() {
        // .gitignore: ignore all *.log files EXCEPT important.log
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join(".gitignore"), "*.log\n!important.log\n").unwrap();
        let random_log = tmp.path().join("random.log");
        let important_log = tmp.path().join("important.log");
        fs::write(&random_log, "x").unwrap();
        fs::write(&important_log, "y").unwrap();

        let ctx = make_ctx_with_root(tmp.path());
        ctx.rebuild_gitignore();

        assert!(is_ignored(&ctx, &random_log));
        assert!(
            !is_ignored(&ctx, &important_log),
            "negation pattern should un-ignore important.log"
        );
    }

    #[test]
    fn rebuild_picks_up_gitignore_changes() {
        let tmp = TempDir::new().unwrap();
        let ignore_path = tmp.path().join(".gitignore");
        fs::write(&ignore_path, "foo.txt\n").unwrap();
        let foo = tmp.path().join("foo.txt");
        let bar = tmp.path().join("bar.txt");
        fs::write(&foo, "").unwrap();
        fs::write(&bar, "").unwrap();

        let ctx = make_ctx_with_root(tmp.path());
        ctx.rebuild_gitignore();
        assert!(is_ignored(&ctx, &foo));
        assert!(!is_ignored(&ctx, &bar));

        // Now flip the rules: ignore bar.txt instead of foo.txt
        fs::write(&ignore_path, "bar.txt\n").unwrap();
        ctx.rebuild_gitignore();
        assert!(!is_ignored(&ctx, &foo));
        assert!(is_ignored(&ctx, &bar));
    }

    #[test]
    fn gitignore_loads_info_exclude_when_present() {
        let tmp = TempDir::new().unwrap();
        let info_dir = tmp.path().join(".git/info");
        fs::create_dir_all(&info_dir).unwrap();
        fs::write(info_dir.join("exclude"), "secrets.txt\n").unwrap();
        let secrets = tmp.path().join("secrets.txt");
        let public = tmp.path().join("public.txt");
        fs::write(&secrets, "token").unwrap();
        fs::write(&public, "ok").unwrap();

        let ctx = make_ctx_with_root(tmp.path());
        ctx.rebuild_gitignore();

        assert!(is_ignored(&ctx, &secrets));
        assert!(!is_ignored(&ctx, &public));
    }

    fn write_fixture_file(root: &Path, relative: &str) -> PathBuf {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, relative).unwrap();
        path
    }

    fn collect_fixture_files(root: &Path, directory: &Path, files: &mut BTreeSet<PathBuf>) {
        for entry in fs::read_dir(directory).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                collect_fixture_files(root, &path, files);
            } else {
                files.insert(path.strip_prefix(root).unwrap().to_path_buf());
            }
        }
    }

    fn assert_matcher_matches_project_walker(ctx: &AppContext, root: &Path) {
        // Standard git filters activate only inside a repository. An empty
        // metadata directory is enough for this fixture and keeps the oracle
        // on the same WalkBuilder path used by project indexing.
        fs::create_dir_all(root.join(".git")).unwrap();
        let mut builder = ignore::WalkBuilder::new(root);
        builder
            .same_file_system(true)
            .standard_filters(true)
            .hidden(false)
            .add_custom_ignore_filename(".aftignore");
        let walked = builder
            .build()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()))
            .map(|entry| entry.path().strip_prefix(root).unwrap().to_path_buf())
            .collect::<BTreeSet<_>>();

        let mut all_files = BTreeSet::new();
        collect_fixture_files(root, root, &mut all_files);
        let matcher_visible = all_files
            .into_iter()
            .filter(|relative| !is_ignored(ctx, &root.join(relative)))
            .collect::<BTreeSet<_>>();

        assert_eq!(
            matcher_visible, walked,
            "watcher matcher and project walker must expose the same files"
        );
    }

    fn nested_wildcard_fixture() -> (TempDir, AppContext) {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join(".gitignore"), "/target/\n").unwrap();
        write_fixture_file(tmp.path(), "crates/lib.rs");
        write_fixture_file(tmp.path(), "docs/x.md");
        write_fixture_file(tmp.path(), "target/output.bin");
        write_fixture_file(tmp.path(), "tests/docker/fixtures/anything");
        fs::write(tmp.path().join("tests/docker/fixtures/.gitignore"), "*\n").unwrap();
        let ctx = make_ctx_with_root(tmp.path());
        (tmp, ctx)
    }

    #[test]
    fn matcher_picks_up_nested_gitignore() {
        let tmp = TempDir::new().unwrap();
        // Root .gitignore is intentionally empty — only the nested one ignores
        fs::write(tmp.path().join(".gitignore"), "").unwrap();
        let sub = tmp.path().join("packages/foo");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join(".gitignore"), "generated/\n").unwrap();
        let generated_file = sub.join("generated").join("out.js");
        fs::create_dir_all(generated_file.parent().unwrap()).unwrap();
        fs::write(&generated_file, "x").unwrap();

        let ctx = make_ctx_with_root(tmp.path());
        ctx.rebuild_gitignore();

        assert!(
            is_ignored(&ctx, &generated_file),
            "nested gitignore in packages/foo/.gitignore should ignore generated/"
        );
    }

    #[test]
    fn nested_wildcard_is_scoped_to_its_directory() {
        with_neutralized_global_gitignore(|| {
            let (tmp, ctx) = nested_wildcard_fixture();
            ctx.rebuild_gitignore();

            assert!(!is_ignored(&ctx, &tmp.path().join("crates/lib.rs")));
            assert!(!is_ignored(&ctx, &tmp.path().join("docs/x.md")));
            assert!(is_ignored(
                &ctx,
                &tmp.path().join("tests/docker/fixtures/anything")
            ));
            assert!(is_ignored(&ctx, &tmp.path().join("target")));
            assert_matcher_matches_project_walker(&ctx, tmp.path());
        });
    }

    #[test]
    fn nested_unanchored_pattern_matches_at_every_depth_below_its_directory() {
        with_neutralized_global_gitignore(|| {
            let tmp = TempDir::new().unwrap();
            fs::create_dir_all(tmp.path().join("sub")).unwrap();
            fs::write(tmp.path().join("sub/.gitignore"), "build\n").unwrap();
            let direct = write_fixture_file(tmp.path(), "sub/build/direct.txt");
            let deep = write_fixture_file(tmp.path(), "sub/deep/build/deep.txt");
            let root_build = write_fixture_file(tmp.path(), "build/root.txt");
            let ctx = make_ctx_with_root(tmp.path());
            ctx.rebuild_gitignore();

            assert!(is_ignored(&ctx, &direct));
            assert!(is_ignored(&ctx, &deep));
            assert!(!is_ignored(&ctx, &root_build));
            assert_matcher_matches_project_walker(&ctx, tmp.path());
        });
    }

    #[test]
    fn nested_anchored_pattern_matches_only_at_its_directory_root() {
        with_neutralized_global_gitignore(|| {
            let tmp = TempDir::new().unwrap();
            fs::create_dir_all(tmp.path().join("sub")).unwrap();
            fs::write(tmp.path().join("sub/.gitignore"), "/build\n").unwrap();
            let direct = write_fixture_file(tmp.path(), "sub/build/direct.txt");
            let deep = write_fixture_file(tmp.path(), "sub/deep/build/deep.txt");
            let ctx = make_ctx_with_root(tmp.path());
            ctx.rebuild_gitignore();

            assert!(is_ignored(&ctx, &direct));
            assert!(!is_ignored(&ctx, &deep));
            assert_matcher_matches_project_walker(&ctx, tmp.path());
        });
    }

    #[test]
    fn nested_negation_only_unignores_files_below_its_directory() {
        with_neutralized_global_gitignore(|| {
            let tmp = TempDir::new().unwrap();
            fs::write(tmp.path().join(".gitignore"), "*.log\n").unwrap();
            fs::create_dir_all(tmp.path().join("sub")).unwrap();
            fs::write(tmp.path().join("sub/.gitignore"), "!keep.log\n").unwrap();
            let nested_keep = write_fixture_file(tmp.path(), "sub/keep.log");
            let root_keep = write_fixture_file(tmp.path(), "keep.log");
            let other = write_fixture_file(tmp.path(), "other.log");
            let ctx = make_ctx_with_root(tmp.path());
            ctx.rebuild_gitignore();

            assert!(!is_ignored(&ctx, &nested_keep));
            assert!(is_ignored(&ctx, &root_keep));
            assert!(is_ignored(&ctx, &other));
            assert_matcher_matches_project_walker(&ctx, tmp.path());
        });
    }

    #[test]
    fn deeper_nested_ignore_file_takes_precedence() {
        with_neutralized_global_gitignore(|| {
            let tmp = TempDir::new().unwrap();
            fs::write(tmp.path().join(".gitignore"), "gen/\n").unwrap();
            fs::create_dir_all(tmp.path().join("a/b")).unwrap();
            fs::write(tmp.path().join("a/.gitignore"), "!gen/\n").unwrap();
            fs::write(tmp.path().join("a/b/.gitignore"), "gen/\n").unwrap();
            let shallow = write_fixture_file(tmp.path(), "a/gen/shallow.txt");
            let deep = write_fixture_file(tmp.path(), "a/b/gen/deep.txt");
            let ctx = make_ctx_with_root(tmp.path());
            ctx.rebuild_gitignore();

            assert!(!is_ignored(&ctx, &shallow));
            assert!(is_ignored(&ctx, &deep));
            assert_matcher_matches_project_walker(&ctx, tmp.path());
        });
    }

    #[test]
    fn nested_aftignore_pattern_is_scoped_to_its_directory() {
        with_neutralized_global_gitignore(|| {
            let tmp = TempDir::new().unwrap();
            fs::create_dir_all(tmp.path().join("sub")).unwrap();
            fs::write(tmp.path().join("sub/.aftignore"), "*\n").unwrap();
            let nested = write_fixture_file(tmp.path(), "sub/nested.txt");
            let root = write_fixture_file(tmp.path(), "root.txt");
            let ctx = make_ctx_with_root(tmp.path());
            ctx.rebuild_gitignore();

            assert!(is_ignored(&ctx, &nested));
            assert!(!is_ignored(&ctx, &root));
            assert_matcher_matches_project_walker(&ctx, tmp.path());
        });
    }

    #[test]
    fn nested_ignore_directory_glob_metacharacters_are_literal() {
        with_neutralized_global_gitignore(|| {
            let tmp = TempDir::new().unwrap();
            fs::create_dir_all(tmp.path().join("foo[1]")).unwrap();
            fs::write(tmp.path().join("foo[1]/.gitignore"), "build\n").unwrap();
            let nested = write_fixture_file(tmp.path(), "foo[1]/build/nested.txt");
            let lookalike = write_fixture_file(tmp.path(), "foo1/build/lookalike.txt");
            let ctx = make_ctx_with_root(tmp.path());
            ctx.rebuild_gitignore();

            assert!(is_ignored(&ctx, &nested));
            assert!(!is_ignored(&ctx, &lookalike));
            assert_matcher_matches_project_walker(&ctx, tmp.path());
        });
    }

    #[test]
    fn nested_wildcard_does_not_poison_watcher_exclusions() {
        with_neutralized_global_gitignore(|| {
            let (tmp, ctx) = nested_wildcard_fixture();
            ctx.rebuild_gitignore();
            let root = fs::canonicalize(tmp.path()).unwrap();

            let exclusions = crate::watcher_filter::derive_excluded_subtrees(
                &root,
                &ctx.shared_gitignore(),
                None,
            );
            let paths = crate::watcher_filter::watcher_exclusion_paths(&exclusions);

            assert!(!paths.contains(&root.join("crates")));
            assert!(!paths.contains(&root.join("docs")));
            assert!(paths.contains(&root.join("target")));
            assert_matcher_matches_project_walker(&ctx, &root);
        });
    }

    #[test]
    fn nested_wildcard_keeps_visible_paths_in_watcher_filter() {
        with_neutralized_global_gitignore(|| {
            let (tmp, ctx) = nested_wildcard_fixture();
            ctx.rebuild_gitignore();
            let root = fs::canonicalize(tmp.path()).unwrap();
            let visible = root.join("crates/lib.rs");
            let ignored = root.join("tests/docker/fixtures/anything");
            let config = crate::watcher_filter::WatcherFilterConfig::new(root.clone(), None);

            let filtered = crate::watcher_filter::filter_watcher_raw_paths_for_test(
                &config,
                &ctx.shared_gitignore(),
                [visible.clone(), ignored],
            );

            assert_eq!(filtered.changed, BTreeSet::from([visible]));
            assert_matcher_matches_project_walker(&ctx, &root);
        });
    }
}

#[cfg(test)]
mod verify_memo_watcher_tests {}

#[cfg(test)]
mod watcher_runtime_state_tests {
    use super::*;
    use crate::language::StubProvider;

    fn test_context() -> AppContext {
        AppContext::new(Box::new(StubProvider), Config::default())
    }

    #[test]
    fn watcher_runtime_requires_both_thread_and_dispatch_receiver() {
        let ctx = test_context();
        let (dispatch_tx, dispatch_rx) = crate::watcher_filter::watcher_dispatch_channel();
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_shutdown = Arc::clone(&shutdown);
        let join = std::thread::spawn(move || {
            while !thread_shutdown.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(1));
            }
            drop(dispatch_tx);
        });
        ctx.install_watcher_runtime(
            dispatch_rx,
            WatcherThreadHandle::new(Arc::clone(&shutdown), join),
        );
        assert!(ctx.watcher_runtime_active());

        *ctx.watcher_rx.lock() = None;
        assert!(
            !ctx.watcher_runtime_active(),
            "a thread without its dispatch receiver is not a usable watcher runtime"
        );
        ctx.stop_watcher_runtime();
    }
}

#[cfg(test)]
mod semantic_probe_tests {
    use super::*;

    #[test]
    fn cleared_semantic_worker_invalidates_orphaned_probe_timer() {
        let root = tempfile::tempdir().unwrap();
        let ctx = AppContext::new(
            default_language_provider_factory(),
            Config {
                project_root: Some(root.path().to_path_buf()),
                ..Config::default()
            },
        );
        let (request_tx, _request_rx) = crossbeam_channel::unbounded();
        let (_event_tx, event_rx) = crossbeam_channel::unbounded();
        let worker_slot = Arc::new(Mutex::new(None));
        ctx.install_semantic_refresh_worker_for_build_epoch(
            request_tx,
            event_rx,
            worker_slot,
            ctx.semantic_index_rx_epoch(),
        );

        ctx.ensure_semantic_refresh_probe_scheduled(Duration::from_millis(20));
        assert!(ctx.semantic_refresh_probe_is_scheduled());
        ctx.clear_semantic_refresh_worker();
        std::thread::sleep(Duration::from_millis(50));

        assert!(!ctx.semantic_refresh_probe_ready());
        assert!(!ctx.semantic_refresh_probe_is_scheduled());
        assert!(!ctx.completion_drains_have_work());
    }
}

#[cfg(test)]
mod worker_rust_cargo_policy_tests {
    use super::*;

    /// Every worker tool call asks to keep Cargo checks off. Once the flag is
    /// set, asking again must not wait for the LSP manager lock, which a
    /// language-server start or a diagnostics request can hold for seconds.
    #[test]
    fn worker_rust_cargo_policy_is_answered_without_the_lsp_lock_once_set() {
        let ctx = AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            Config::default(),
        );
        assert!(!ctx.lsp().rust_cargo_disabled_for_worker());
        ctx.disable_rust_cargo_for_worker();
        assert!(ctx.lsp().rust_cargo_disabled_for_worker());

        let held = ctx.lsp();
        let (answered_tx, answered_rx) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                ctx.disable_rust_cargo_for_worker();
                let _ = answered_tx.send(());
            });
            let answered = answered_rx
                .recv_timeout(std::time::Duration::from_secs(10))
                .is_ok();
            drop(held);
            assert!(
                answered,
                "a repeated worker call waited for the LSP manager lock"
            );
        });
    }
}

#[cfg(test)]
mod directory_arrival_pending_tests {
    use super::*;

    #[test]
    fn watcher_directory_discovery_pending_survives_handoff() {
        let root = tempfile::tempdir().unwrap();
        let ctx = AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                project_root: Some(root.path().to_path_buf()),
                ..Default::default()
            },
        );
        let counters = ctx.watcher_counters();
        let (tx, rx) = crossbeam_channel::unbounded();
        *ctx.watcher_rx().lock() = Some(rx);
        let discovery = counters.directory_arrival_guard();
        assert!(ctx.watcher_query_has_pending_changes());
        counters.note_directory_files_queued();
        drop(discovery);
        assert!(ctx.watcher_query_has_pending_changes());
        tx.send(crate::watcher_filter::WatcherDispatchEvent::Paths(vec![
            root.path().join("new.rs"),
        ]))
        .unwrap();
        counters.clear_unflushed_paths();
        assert!(
            ctx.watcher_query_has_pending_changes(),
            "dispatch must take over before discovery stops being pending"
        );
        ctx.watcher_rx()
            .lock()
            .as_ref()
            .unwrap()
            .try_recv()
            .unwrap();
        assert!(!ctx.watcher_query_has_pending_changes());
        let empty_probe = counters.directory_arrival_guard();
        drop(empty_probe);
        assert!(
            !ctx.watcher_query_has_pending_changes(),
            "an empty arrival leaves no pending files"
        );
    }
}

#[cfg(test)]
mod data_layer_health_tests {
    use super::*;

    fn summary(search: &'static str, callgraph: &'static str) -> RootHealthSummary {
        RootHealthSummary {
            state: RootHealthState::Ready,
            search_index_status: Some(search),
            semantic_index: Some(SemanticHealthComponentSnapshot {
                status: "disabled",
                reason: None,
                since_ms: None,
                next_retry_ms: None,
                paused_until_ms: None,
                stage: None,
                embedded_chunks: None,
                total_chunks: None,
                current_batch: None,
                total_batches: None,
                fill: None,
            }),
            callgraph_store_status: Some(callgraph),
            tier2_status: Some("ready"),
            ..RootHealthSummary::busy()
        }
    }

    #[test]
    fn a_root_nothing_queried_is_settled_and_a_building_plane_is_not() {
        // `off` is the data layer's plane on a root whose checkout nothing
        // has bound: settled, like `disabled`.
        assert!(summary("off", "off").is_fully_ready());
        assert!(summary("off", "off")
            .into_snapshot(Path::new("/r"))
            .is_fully_ready());
        for (search, callgraph) in [("building", "ready"), ("ready", "unavailable")] {
            assert!(!summary(search, callgraph).is_fully_ready());
            assert!(!summary(search, callgraph)
                .into_snapshot(Path::new("/r"))
                .is_fully_ready());
        }
    }
}
