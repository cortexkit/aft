use crate::cache_freshness::{self, FileFreshness, FreshnessVerdict};
use crate::config::{
    SemanticBackend, SemanticBackendConfig, DEFAULT_SEMANTIC_QUERY_TIMEOUT_MS,
    MAX_SEMANTIC_QUERY_TIMEOUT_MS, MIN_SEMANTIC_QUERY_TIMEOUT_MS,
};
use crate::context::SemanticIndexStatus;
use crate::fs_lock;
use crate::parser::{detect_language, extract_symbols_from_tree, parse_source_with_cached_parser};
use crate::search_index::{cache_relative_path, cached_path_under_root};
use crate::symbols::{Symbol, SymbolKind};
use crate::synapse_embed::SynapseEmbeddingClient;
use crate::{slog_info, slog_warn};

use crate::local_embed::LocalEmbedder;
use rayon::prelude::*;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::env;
use std::error::Error;
use std::fmt::Display;
use std::fs::{self, OpenOptions};
use std::io::{self, BufReader, BufWriter, Cursor, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant, SystemTime};
use url::Url;

const DEFAULT_DIMENSION: usize = 384;
const MAX_ENTRIES: usize = 1_000_000;
// Covers high-dimensional backends such as OpenAI text-embedding-3-large (3072)
// and common local models (4096) while keeping a bounded supported shape.
const MAX_DIMENSION: usize = 4096;
const F32_BYTES: usize = std::mem::size_of::<f32>();
const HEADER_BYTES_V1: usize = 9;
const HEADER_BYTES_V2: usize = 13;
// One retry schedule for the cold semantic build: configure's build loop sleeps
// on it and the health status reports the deadline it produces, so the two can
// never drift apart.
const BUILD_BACKEND_RETRY_SCHEDULE_SECS: [u64; 3] = [15, 30, 60];
// A parked status outlives its retry deadline by this much before it is
// treated as abandoned. The retry itself re-walks and re-chunks the corpus
// before it reaches the backend, which takes tens of seconds on large roots,
// so the grace must cover a whole attempt; a loop that exits clears the
// status explicitly, so the grace only ever bounds a loop that died.
const BUILD_BACKEND_STATUS_EXPIRY_GRACE_MS: u64 = 5 * 60 * 1_000;

#[derive(Clone, Debug)]
pub(crate) struct EmbeddingBackendBuildHealth {
    pub(crate) last_error: String,
    pub(crate) since_ms: u64,
    pub(crate) next_retry_ms: u64,
    failures: usize,
}

fn embedding_backend_build_health_registry(
) -> &'static Mutex<HashMap<PathBuf, EmbeddingBackendBuildHealth>> {
    static REGISTRY: OnceLock<Mutex<HashMap<PathBuf, EmbeddingBackendBuildHealth>>> =
        OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn unix_millis_now() -> u64 {
    SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

/// Delay before the cold semantic build retries an unreachable embedding backend.
/// `AFT_SEMANTIC_RETRY_BACKOFF_MS` is a test seam that shrinks the schedule so
/// recovery integration tests do not wait real 15s+ windows; not a user knob.
pub(crate) fn build_backend_retry_delay_ms(attempt: usize) -> u64 {
    if let Ok(raw) = env::var("AFT_SEMANTIC_RETRY_BACKOFF_MS") {
        if let Ok(ms) = raw.parse::<u64>() {
            return ms;
        }
    }
    BUILD_BACKEND_RETRY_SCHEDULE_SECS
        .get(attempt)
        .copied()
        .unwrap_or(*BUILD_BACKEND_RETRY_SCHEDULE_SECS.last().unwrap())
        .saturating_mul(1_000)
}

fn record_embedding_backend_build_failure(project_root: &Path, error: &str) {
    let now_ms = unix_millis_now();
    let clean = strip_transient_embedding_marker(error);
    let mut registry = embedding_backend_build_health_registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let entry = registry
        .entry(project_root.to_path_buf())
        .or_insert_with(|| EmbeddingBackendBuildHealth {
            last_error: clean.clone(),
            since_ms: now_ms,
            next_retry_ms: now_ms,
            failures: 0,
        });
    entry.last_error = clean;
    entry.next_retry_ms = now_ms.saturating_add(build_backend_retry_delay_ms(entry.failures));
    entry.failures = entry.failures.saturating_add(1);
}

/// The retry loop owns the sleep, so it stamps the deadline it will actually
/// sleep to. The builder's own record (above) runs first with its schedule
/// position; this overrides it with the loop's, which is the one that matters
/// when the two counters differ (a loop inherits an entry from a superseded
/// loop, or a builder outside the loop recorded the failure).
pub(crate) fn record_embedding_backend_retry_deadline(
    project_root: &Path,
    error: &str,
    backoff: std::time::Duration,
) {
    let now_ms = unix_millis_now();
    let clean = strip_transient_embedding_marker(error);
    let backoff_ms = u64::try_from(backoff.as_millis()).unwrap_or(u64::MAX);
    let mut registry = embedding_backend_build_health_registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let entry = registry
        .entry(project_root.to_path_buf())
        .or_insert_with(|| EmbeddingBackendBuildHealth {
            last_error: clean.clone(),
            since_ms: now_ms,
            next_retry_ms: now_ms,
            failures: 0,
        });
    entry.last_error = clean;
    entry.next_retry_ms = now_ms.saturating_add(backoff_ms);
}

/// Called on every exit path of a cold-build retry loop so a parked status
/// never outlives the loop that would have retried it.
pub(crate) fn clear_embedding_backend_retry_status(project_root: &Path) {
    clear_embedding_backend_build_failure(project_root);
}

fn clear_embedding_backend_build_failure(project_root: &Path) {
    embedding_backend_build_health_registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(project_root);
}

pub(crate) fn embedding_backend_build_health(
    project_root: &Path,
) -> Option<EmbeddingBackendBuildHealth> {
    let now_ms = unix_millis_now();
    let mut registry = embedding_backend_build_health_registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let expired = registry.get(project_root).is_some_and(|health| {
        now_ms
            > health
                .next_retry_ms
                .saturating_add(BUILD_BACKEND_STATUS_EXPIRY_GRACE_MS)
    });
    if expired {
        registry.remove(project_root);
        return None;
    }
    registry.get(project_root).cloned()
}

#[cfg(test)]
pub(crate) fn record_embedding_backend_build_failure_for_test(project_root: &Path, reason: &str) {
    record_embedding_backend_build_failure(
        project_root,
        &format!("{TRANSIENT_EMBEDDING_MARKER}{reason}"),
    );
}

fn begin_semantic_index_build(
    project_root: &Path,
) -> (
    Option<crate::logging::IndexBuildGuard>,
    crate::logging::IndexBuildScope,
    crate::logging::IndexBuildFailureGuard,
) {
    // The parked-backoff status is deliberately kept while a retry runs: the
    // attempt re-walks and re-chunks the corpus before it touches the backend,
    // and clearing here read as "building" for that whole window and reset the
    // failure count every cycle. Success or a non-transient failure clears it
    // in `finish_semantic_index_build`.
    if let Some(scope) = crate::logging::current_index_build() {
        if scope.plane == crate::logging::IndexPlane::Semantic {
            return (None, scope, crate::logging::IndexBuildFailureGuard::new());
        }
    }
    let key = crate::search_index::artifact_cache_key(project_root);
    let scope = crate::logging::IndexBuildScope::new(
        crate::logging::IndexPlane::Semantic,
        project_root,
        key,
    );
    let guard = crate::logging::install_index_build(scope.clone());
    crate::logging::log_index_event(crate::logging::IndexEvent::from_scope(
        crate::logging::IndexEventKind::BuildStarted,
        &scope,
    ));
    (
        Some(guard),
        scope,
        crate::logging::IndexBuildFailureGuard::new(),
    )
}

fn finish_semantic_index_build(
    scope: &crate::logging::IndexBuildScope,
    failure_guard: &mut crate::logging::IndexBuildFailureGuard,
    result: &Result<SemanticIndex, String>,
) {
    match result {
        Ok(index) => {
            clear_embedding_backend_build_failure(&scope.root);
            crate::logging::log_index_event(
                crate::logging::IndexEvent::from_scope(
                    crate::logging::IndexEventKind::BuildReady,
                    scope,
                )
                .field("elapsed_ms", scope.elapsed_ms())
                .field("files", index.file_mtimes.len())
                .field("chunks", index.entries.len())
                .field("skipped_rows", index.skipped_rows),
            );
            failure_guard.disarm();
        }
        Err(error) if error.contains("superseded") => {
            clear_embedding_backend_build_failure(&scope.root);
            crate::logging::log_index_event(
                crate::logging::IndexEvent::from_scope(
                    crate::logging::IndexEventKind::BuildSuperseded,
                    scope,
                )
                .field("stage", "embed"),
            );
            failure_guard.disarm();
        }
        Err(error) => {
            if embedding_failure_is_transient(error) {
                record_embedding_backend_build_failure(&scope.root, error);
            } else {
                clear_embedding_backend_build_failure(&scope.root);
            }
            crate::logging::log_index_event(
                crate::logging::IndexEvent::from_scope(
                    crate::logging::IndexEventKind::BuildFailed,
                    scope,
                )
                .field("reason", error),
            );
            failure_guard.disarm();
        }
    }
}

/// Every `semantic_index.status` word the daemon can put on the wire, named in
/// one place so a reader can be checked against it.
///
/// The producers, all of which map into this set:
/// - `commands/status.rs`: `busy` (status lock contention), `disabled`,
///   `loading` (a cold build in progress), `ready`, `failed`,
///   `backend_unavailable`, and — when an index object is already loaded —
///   whatever [`SemanticIndex::status_label`] reports for the daemon-held
///   status (`disabled`, `loading`, `failed`, `empty`, or `ready`).
/// - the root-health snapshot in `context.rs`: `backend_unavailable`, `ready`,
///   `building`, `disabled`, `degraded`.
/// - semantic search replies: `ready`, `building`, `disabled`, `unavailable`.
///
/// A reader that does not recognise a word has nothing to render but the raw
/// word itself, which is how `backend_unavailable` reached users as grey
/// unexplained text. packages/opencode-plugin/src/shared/status.ts checks its
/// own mapping against this list, so adding a word here without teaching the
/// sidebar about it fails that test instead of shipping.
pub const SEMANTIC_INDEX_STATUS_WORDS: &[&str] = &[
    "backend_unavailable",
    "building",
    "busy",
    "degraded",
    "disabled",
    "empty",
    "failed",
    "loading",
    "ready",
    "unavailable",
];

/// Opening words of every missing-runtime message this crate produces.
///
/// `is_onnx_runtime_unavailable` treats this prefix as proof on its own, and
/// readers outside the daemon (the OpenCode sidebar) key on the same prefix to
/// tell "the runtime is not installed" apart from an ordinary backend failure.
/// Changing it changes that contract.
pub const ONNX_RUNTIME_MISSING_PREFIX: &str = "ONNX Runtime not found.";

/// What the daemon tells a user whose ONNX Runtime is missing.
///
/// It deliberately names no per-platform install command. Whether AFT can fetch
/// the runtime itself is answered in exactly one place — the downloader's
/// platform table behind `isOrtAutoDownloadSupported`
/// (packages/aft-bridge/src/onnx-runtime.ts), which
/// packages/aft-cli/src/lib/onnx.ts asks before it prints any manual
/// instruction. The daemon cannot consult that table from here, and guessing it
/// is how the old hint came to recommend Homebrew to Apple Silicon users whose
/// runtime AFT downloads for them, in the same sentence as saying the download
/// is automatic. `doctor --fix` is the command that asks the owner: it installs
/// the runtime where AFT can fetch one and prints the manual route where it
/// cannot, so pointing at it is the whole answer on every platform.
///
/// Starts with `ONNX_RUNTIME_MISSING_PREFIX` so readers can classify it; a test
/// holds the two together.
const ONNX_RUNTIME_INSTALL_HINT: &str =
    "ONNX Runtime not found. Run `npx @cortexkit/aft doctor --fix`: it installs \
     the runtime where AFT can download one for this platform and prints the \
     manual install command where it cannot.";

const SEMANTIC_INDEX_VERSION_V1: u8 = 1;
const SEMANTIC_INDEX_VERSION_V2: u8 = 2;
/// V3 adds subsec_nanos to the file-mtime table so staleness detection survives
/// restart round-trips on filesystems with subsecond mtime precision (APFS,
/// ext4 with nsec, NTFS). V1/V2 persisted whole-second mtimes only, which
/// caused every restart to flag ~99% of files as stale and re-embed them.
const SEMANTIC_INDEX_VERSION_V3: u8 = 3;
/// V4 keeps the V3 on-disk layout but rebuilds persisted snippets once after
/// fixing symbol ranges that were incorrectly treated as 1-based.
const SEMANTIC_INDEX_VERSION_V4: u8 = 4;
/// V5 adds file sizes to the file metadata table so incremental staleness
/// detection can catch content changes even when mtime precision misses them.
const SEMANTIC_INDEX_VERSION_V5: u8 = 5;
/// V6 stores paths relative to project_root and adds content hashes.
const SEMANTIC_INDEX_VERSION_V6: u8 = 6;
/// V7 adds qualified symbol names for ranking metadata without changing embeddings.
const SEMANTIC_INDEX_VERSION_V7: u8 = 7;
/// A V6/V7 base snapshot may be followed by these checksummed delta frames.
/// The base stays independently readable, so an incomplete final frame can be discarded.
const SEMANTIC_SEGMENT_MAGIC: &[u8; 8] = b"AFTSEG01";
const SEMANTIC_SEGMENT_VERSION: u8 = 1;
const SEMANTIC_SEGMENT_FRAME_HEADER_BYTES: usize = 8 + 8 + 32;
const SEMANTIC_COMPACT_SEGMENT_LIMIT: usize = 64;
const SEMANTIC_COMPACT_BYTE_RATIO_DENOMINATOR: u64 = 4;
const SEMANTIC_PERSIST_LOCK_MIN_WAIT: Duration = Duration::from_secs(5);
const SEMANTIC_PERSIST_LOCK_BYTES_PER_SECOND: u64 = 32 * 1024 * 1024;
const DEFAULT_OPENAI_EMBEDDING_PATH: &str = "/embeddings";
const DEFAULT_OLLAMA_EMBEDDING_PATH: &str = "/api/embed";
// Build/refresh embedding requests keep a larger budget because they run on
// background workers and often batch many texts through a cold local backend.
const DEFAULT_OPENAI_EMBEDDING_TIMEOUT_MS: u64 = 25_000;
const DEFAULT_MAX_BATCH_SIZE: usize = 64;
const QUERY_EMBEDDING_CACHE_CAP: usize = 1_000;
const QUERY_EMBED_HEALTH_SAMPLE_CAP: usize = 1_000;
const QUERY_EMBED_OK_LOG_INTERVAL: Duration = Duration::from_secs(60);
const FALLBACK_BACKEND: &str = "none";
const EMBEDDING_REQUEST_MAX_ATTEMPTS: usize = 3;
const EMBEDDING_REQUEST_BACKOFF_MS: [u64; 2] = [500, 1_000];
const BUILD_EMBEDDING_TIMEOUT_MARKER_PREFIX: &str = "[build-timeout:";
const BUILD_EMBEDDING_TIMEOUT_MARKER_SUFFIX: &str = "]";
const ROW_TOO_LONG_MARKER_PREFIX: &str = "[row-too-long:";
const ROW_TOO_LONG_MARKER_SUFFIX: &str = "]";
const MAX_ROW_SHRINK_ATTEMPTS: usize = 4;
const ROW_SHRINK_RATIO_MARGIN: f64 = 0.9;
const BUILD_PER_ITEM_EMA_ALPHA: f64 = 0.25;
const BUILD_PER_ITEM_SAFETY_FACTOR: f64 = 2.0;
const BUILD_INITIAL_BATCH_DIVISOR: u64 = 16;
const BUILD_BATCH_GROWTH_SUCCESSES: usize = 2;
static SEMANTIC_LOCK_ACQUIRE_MUTEX: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone)]
struct BuildEmbeddingRowMetadata {
    embedded_text: String,
    skipped_reason: Option<String>,
}

struct AdaptiveBuildRow {
    metadata: BuildEmbeddingRowMetadata,
    vector: Option<Vec<f32>>,
}

thread_local! {
    /// `EmbeddingModel::embed` keeps its long-standing vector-only API. The
    /// semantic builder consumes this same-thread metadata immediately after
    /// each HTTP build call so it can persist shrunk text and omit skipped rows.
    static LAST_HTTP_BUILD_METADATA: RefCell<Option<Vec<BuildEmbeddingRowMetadata>>> = const {
        RefCell::new(None)
    };
    #[cfg(test)]
    static TEST_SKIPPED_ROW_WARNINGS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    #[cfg(test)]
    static TEST_QUERY_BUDGET_MS: RefCell<Option<u64>> = const { RefCell::new(None) };
}

fn clear_http_build_metadata() {
    LAST_HTTP_BUILD_METADATA.with(|slot| slot.borrow_mut().take());
}

fn set_http_build_metadata(metadata: Vec<BuildEmbeddingRowMetadata>) {
    LAST_HTTP_BUILD_METADATA.with(|slot| *slot.borrow_mut() = Some(metadata));
}

fn take_http_build_metadata() -> Option<Vec<BuildEmbeddingRowMetadata>> {
    LAST_HTTP_BUILD_METADATA.with(|slot| slot.borrow_mut().take())
}

/// Test-only probe counter for the managed-ONNX resolver (see
/// `find_managed_onnx_runtime`). Counts storage-tree reads so a negative-control
/// test can assert a pre-set ORT_DYLIB_PATH short-circuits the resolver.
#[cfg(test)]
static MANAGED_ORT_PROBE_READS: AtomicUsize = AtomicUsize::new(0);

/// Per-query request policy kept separate from the background build timeout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueryBudget {
    timeout_ms: u64,
}

impl QueryBudget {
    pub fn from_config(config: &SemanticBackendConfig) -> Self {
        #[cfg(test)]
        if let Some(timeout_ms) = TEST_QUERY_BUDGET_MS.with(|slot| *slot.borrow()) {
            return Self { timeout_ms };
        }
        let configured = if config.query_timeout_ms == 0 {
            DEFAULT_SEMANTIC_QUERY_TIMEOUT_MS
        } else {
            config.query_timeout_ms
        };
        Self {
            timeout_ms: configured
                .clamp(MIN_SEMANTIC_QUERY_TIMEOUT_MS, MAX_SEMANTIC_QUERY_TIMEOUT_MS),
        }
    }

    #[cfg(test)]
    fn timeout_ms(self) -> u64 {
        self.timeout_ms
    }
}

#[cfg(test)]
pub(crate) fn with_query_budget_for_test<R>(timeout_ms: u64, action: impl FnOnce() -> R) -> R {
    struct Reset(Option<u64>);
    impl Drop for Reset {
        fn drop(&mut self) {
            TEST_QUERY_BUDGET_MS.with(|slot| *slot.borrow_mut() = self.0);
        }
    }

    let previous = TEST_QUERY_BUDGET_MS.with(|slot| slot.borrow_mut().replace(timeout_ms));
    let _reset = Reset(previous);
    action()
}

#[derive(Debug, Clone, Copy)]
struct BuildRequestBudget {
    batch_size: usize,
    deadline_ms: u64,
}

#[derive(Debug, Clone, Copy)]
enum EmbeddingRequestPolicy {
    Build(BuildRequestBudget),
    Query(QueryBudget),
}

impl EmbeddingRequestPolicy {
    fn max_attempts(self) -> usize {
        match self {
            Self::Build(_) => EMBEDDING_REQUEST_MAX_ATTEMPTS,
            Self::Query(_) => 1,
        }
    }

    fn request_timeout(self) -> Duration {
        match self {
            Self::Build(budget) => Duration::from_millis(budget.deadline_ms),
            Self::Query(budget) => Duration::from_millis(budget.timeout_ms),
        }
    }
}

pub struct SemanticIndexLock {
    _guard: Option<fs_lock::LockGuard>,
}

impl SemanticIndexLock {
    pub fn acquire(
        storage_dir: &Path,
        project_key: &str,
        project_root: &Path,
    ) -> std::io::Result<Self> {
        let dir = storage_dir.join("semantic").join(project_key);
        let path = dir.join("cache.lock");
        let access = crate::root_cache::ArtifactAccess::for_root(project_root);
        if !access.allows_write(project_key, &path) {
            return Ok(Self { _guard: None });
        }
        fs::create_dir_all(&dir)?;
        let _acquire_guard = SEMANTIC_LOCK_ACQUIRE_MUTEX
            .lock()
            .map_err(|_| std::io::Error::other("semantic cache lock acquisition mutex poisoned"))?;
        fs_lock::try_acquire(&path, Duration::from_secs(2))
            .map(|guard| Self {
                _guard: Some(guard),
            })
            .map_err(|error| match error {
                fs_lock::AcquireError::Timeout => {
                    std::io::Error::other("timed out acquiring semantic cache lock")
                }
                fs_lock::AcquireError::Io(error) => error,
            })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SemanticIndexFingerprint {
    pub backend: String,
    pub model: String,
    #[serde(default)]
    pub base_url: String,
    pub dimension: usize,
    #[serde(default = "default_chunking_version")]
    pub chunking_version: u32,
    /// Exact caps used to construct symbol embedding rows. Including them in the
    /// fingerprint prevents cache reuse across incompatible chunk shapes.
    #[serde(default)]
    pub embed_text_caps: EmbedTextCaps,
    /// The Synapse fingerprint and table epoch identify the served vector space
    /// so indexes built against incompatible embeddings are rejected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synapse_fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synapse_table_epoch: Option<u64>,
    /// Alternative fingerprints that Synapse explicitly declares equivalent to
    /// this index's fingerprint, allowing those versions to pass compatibility checks.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub synapse_equivalent_to: Vec<String>,
}

fn default_chunking_version() -> u32 {
    2
}

impl SemanticIndexFingerprint {
    fn from_config(config: &SemanticBackendConfig, dimension: usize) -> Self {
        // Use normalized URL for fingerprinting so cosmetic differences
        // (e.g. "http://host/v1" vs "http://host/v1/") don't cause rebuilds.
        let base_url = config
            .base_url
            .as_ref()
            .and_then(|u| normalize_base_url(u).ok())
            .unwrap_or_else(|| FALLBACK_BACKEND.to_string());
        Self {
            backend: config.backend.as_str().to_string(),
            model: config.model.clone(),
            base_url,
            dimension,
            chunking_version: default_chunking_version(),
            embed_text_caps: EmbedTextCaps::from_config(config),
            synapse_fingerprint: None,
            synapse_table_epoch: None,
            synapse_equivalent_to: Vec::new(),
        }
    }

    pub fn as_string(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| String::new())
    }

    pub(crate) fn for_config_dimension(config: &SemanticBackendConfig, dimension: usize) -> Self {
        Self::from_config(config, dimension)
    }

    fn matches_expected(&self, expected: &str) -> bool {
        let Ok(current) = serde_json::from_str::<Self>(expected) else {
            return false;
        };
        self.matches(&current)
    }

    fn matches(&self, current: &Self) -> bool {
        if self.backend != current.backend
            || self.model != current.model
            || self.base_url != current.base_url
            || self.dimension != current.dimension
            || self.chunking_version != current.chunking_version
            || self.embed_text_caps != current.embed_text_caps
            || self.synapse_table_epoch != current.synapse_table_epoch
        {
            return false;
        }
        match (&self.synapse_fingerprint, &current.synapse_fingerprint) {
            (None, None) => true,
            (Some(cached), Some(served)) => {
                cached == served
                    || current
                        .synapse_equivalent_to
                        .iter()
                        .any(|alias| alias == cached)
                    || self
                        .synapse_equivalent_to
                        .iter()
                        .any(|alias| alias == served)
            }
            _ => false,
        }
    }
}

fn redacted_base_url_host(base_url: &str) -> String {
    if base_url.is_empty() {
        return "<empty>".to_string();
    }
    if base_url == FALLBACK_BACKEND {
        return FALLBACK_BACKEND.to_string();
    }

    match Url::parse(base_url) {
        Ok(parsed) => {
            let host = parsed.host_str().unwrap_or("<missing-host>");
            match parsed.port() {
                Some(port) => format!("{host}:{port}"),
                None => host.to_string(),
            }
        }
        Err(_) => "<invalid>".to_string(),
    }
}

fn format_fingerprint_mismatch_details(
    cached: Option<&SemanticIndexFingerprint>,
    current: &SemanticIndexFingerprint,
) -> String {
    let Some(cached) = cached else {
        return format!(
            "cached fingerprint missing; current backend kind={}, model={}, base_url host={}, dimension={}, chunking version={}",
            current.backend,
            current.model,
            redacted_base_url_host(&current.base_url),
            current.dimension,
            current.chunking_version,
        );
    };

    let mut diffs = Vec::new();
    if cached.backend != current.backend {
        diffs.push(format!(
            "backend kind cached={} current={}",
            cached.backend, current.backend
        ));
    }
    if cached.model != current.model {
        diffs.push(format!(
            "model cached={} current={}",
            cached.model, current.model
        ));
    }
    if cached.base_url != current.base_url {
        let cached_host = redacted_base_url_host(&cached.base_url);
        let current_host = redacted_base_url_host(&current.base_url);
        if cached_host == current_host {
            diffs.push(format!(
                "base_url host cached={} current={} (credentials/path redacted)",
                cached_host, current_host
            ));
        } else {
            diffs.push(format!(
                "base_url host cached={} current={}",
                cached_host, current_host
            ));
        }
    }
    if cached.dimension != current.dimension {
        diffs.push(format!(
            "dimension cached={} current={}",
            cached.dimension, current.dimension
        ));
    }
    if cached.chunking_version != current.chunking_version {
        diffs.push(format!(
            "chunking version cached={} current={}",
            cached.chunking_version, current.chunking_version
        ));
    }
    if cached.embed_text_caps != current.embed_text_caps {
        diffs.push(format!(
            "embed text caps cached={:?} current={:?}",
            cached.embed_text_caps, current.embed_text_caps
        ));
    }
    if cached.synapse_table_epoch != current.synapse_table_epoch {
        diffs.push(format!(
            "synapse table_epoch cached={:?} current={:?}",
            cached.synapse_table_epoch, current.synapse_table_epoch
        ));
    }
    if !cached.matches(current)
        && (cached.synapse_fingerprint.is_some() || current.synapse_fingerprint.is_some())
    {
        diffs.push(format!(
            "synapse fingerprint cached={} current={} (equivalence class checked)",
            cached.synapse_fingerprint.as_deref().unwrap_or("<missing>"),
            current
                .synapse_fingerprint
                .as_deref()
                .unwrap_or("<missing>")
        ));
    }

    if diffs.is_empty() {
        "fingerprint strings differ but parsed fields match".to_string()
    } else {
        diffs.join("; ")
    }
}

fn log_fingerprint_mismatch(cached: Option<&SemanticIndexFingerprint>, expected: &str) {
    match serde_json::from_str::<SemanticIndexFingerprint>(expected) {
        Ok(current) => slog_warn!(
            "cached semantic index fingerprint mismatch, rebuilding without deleting the shared artifact: {}",
            format_fingerprint_mismatch_details(cached, &current)
        ),
        Err(error) => slog_warn!(
            "cached semantic index fingerprint mismatch, rebuilding without deleting the shared artifact: could not parse current fingerprint: {}",
            error
        ),
    }
}

pub(crate) trait LocalEmbeddingProvider: Send {
    fn embed(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>, String>;
}

impl LocalEmbeddingProvider for LocalEmbedder {
    fn embed(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        LocalEmbedder::embed(self, texts)
    }
}

type SharedLocalEmbeddingProvider = Arc<Mutex<Box<dyn LocalEmbeddingProvider>>>;

#[derive(Default)]
struct QueryEmbeddingCache {
    query_embedding_cache: HashMap<String, Vec<f32>>,
    query_embedding_cache_order: VecDeque<String>,
    hits: u64,
    misses: u64,
}

impl QueryEmbeddingCache {
    fn insert(&mut self, query: String, vector: Vec<f32>) {
        if self.query_embedding_cache.contains_key(&query) {
            return;
        }
        if self.query_embedding_cache.len() >= QUERY_EMBEDDING_CACHE_CAP {
            if let Some(oldest) = self.query_embedding_cache_order.pop_front() {
                self.query_embedding_cache.remove(&oldest);
            }
        }
        self.query_embedding_cache.insert(query.clone(), vector);
        self.query_embedding_cache_order.push_back(query);
    }
}

struct LocalQueryEmbedRequest {
    texts: Vec<String>,
    cache_key: String,
    response: crossbeam_channel::Sender<Result<Vec<Vec<f32>>, String>>,
}

struct LocalQueryEmbedWorker {
    requests: crossbeam_channel::Sender<LocalQueryEmbedRequest>,
    busy: Arc<AtomicBool>,
}

impl LocalQueryEmbedWorker {
    fn start(
        model: SharedLocalEmbeddingProvider,
        query_cache: Arc<Mutex<QueryEmbeddingCache>>,
    ) -> Result<Self, String> {
        let (requests, receiver) = crossbeam_channel::unbounded::<LocalQueryEmbedRequest>();
        let busy = Arc::new(AtomicBool::new(false));
        let worker_busy = Arc::clone(&busy);
        std::thread::Builder::new()
            .name("aft-local-query-embed".to_string())
            .spawn(move || {
                while let Ok(request) = receiver.recv() {
                    let result = model
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .embed(&request.texts)
                        .map_err(|error| format!("failed to embed batch: {error}"));
                    if let Ok(vectors) = &result {
                        if let Some(vector) = vectors.first() {
                            query_cache
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .insert(request.cache_key, vector.clone());
                        }
                    }
                    worker_busy.store(false, Ordering::Release);
                    let _ = request.response.send(result);
                }
            })
            .map_err(|error| format!("failed to start local query embed worker: {error}"))?;
        Ok(Self { requests, busy })
    }

    fn try_submit(
        &self,
        texts: Vec<String>,
        cache_key: String,
    ) -> Option<crossbeam_channel::Receiver<Result<Vec<Vec<f32>>, String>>> {
        if self
            .busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return None;
        }
        let (response, receiver) = crossbeam_channel::bounded(1);
        if self
            .requests
            .send(LocalQueryEmbedRequest {
                texts,
                cache_key,
                response,
            })
            .is_err()
        {
            self.busy.store(false, Ordering::Release);
            return None;
        }
        Some(receiver)
    }
}

struct LocalEmbeddingEngine {
    model: SharedLocalEmbeddingProvider,
    query_worker: LocalQueryEmbedWorker,
}

impl LocalEmbeddingEngine {
    fn new(
        model: Box<dyn LocalEmbeddingProvider>,
        query_cache: Arc<Mutex<QueryEmbeddingCache>>,
    ) -> Result<Self, String> {
        let model = Arc::new(Mutex::new(model));
        let query_worker = LocalQueryEmbedWorker::start(Arc::clone(&model), query_cache)?;
        Ok(Self {
            model,
            query_worker,
        })
    }
}

#[derive(Default)]
struct QueryEmbedHealthState {
    timeouts: u64,
    elapsed_ms: VecDeque<u64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct QueryEmbedHealthSnapshot {
    pub(crate) query_embed_timeouts: u64,
    pub(crate) query_embed_p50_ms: u64,
}

fn query_embed_health_registry() -> &'static Mutex<HashMap<PathBuf, QueryEmbedHealthState>> {
    static REGISTRY: OnceLock<Mutex<HashMap<PathBuf, QueryEmbedHealthState>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn record_query_embed_observation(root: Option<&Path>, elapsed_ms: u64, timed_out: bool) {
    let Some(root) = root else {
        return;
    };
    let mut registry = query_embed_health_registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let state = registry.entry(root.to_path_buf()).or_default();
    if timed_out {
        state.timeouts = state.timeouts.saturating_add(1);
    }
    if state.elapsed_ms.len() >= QUERY_EMBED_HEALTH_SAMPLE_CAP {
        state.elapsed_ms.pop_front();
    }
    state.elapsed_ms.push_back(elapsed_ms);
}

pub(crate) fn query_embed_health_snapshot(root: &Path) -> QueryEmbedHealthSnapshot {
    let registry = query_embed_health_registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(state) = registry.get(root) else {
        return QueryEmbedHealthSnapshot::default();
    };
    let mut elapsed_ms = state.elapsed_ms.iter().copied().collect::<Vec<_>>();
    elapsed_ms.sort_unstable();
    let query_embed_p50_ms = elapsed_ms
        .get(elapsed_ms.len().saturating_sub(1) / 2)
        .copied()
        .unwrap_or(0);
    QueryEmbedHealthSnapshot {
        query_embed_timeouts: state.timeouts,
        query_embed_p50_ms,
    }
}

#[cfg(test)]
pub(crate) fn record_query_embed_observation_for_test(
    root: &Path,
    elapsed_ms: u64,
    timed_out: bool,
) {
    record_query_embed_observation(Some(root), elapsed_ms, timed_out);
}

enum SemanticEmbeddingEngine {
    /// Local ONNX embedder (all-MiniLM-L6-v2 via raw `ort`). The config-facing
    /// backend string stays "fastembed" for index-fingerprint compatibility.
    Local(LocalEmbeddingEngine),
    OpenAiCompatible {
        client: Client,
        model: String,
        base_url: String,
        api_key: Option<String>,
    },
    Ollama {
        client: Client,
        model: String,
        base_url: String,
    },
    Synapse(SynapseEmbeddingClient),
}

pub struct SemanticEmbeddingModel {
    backend: SemanticBackend,
    model: String,
    base_url: Option<String>,
    timeout_ms: u64,
    max_batch_size: usize,
    adaptive_build_batch_size: usize,
    successful_build_batches_at_size: usize,
    per_item_ema_ms: Option<f64>,
    dimension: Option<usize>,
    engine: SemanticEmbeddingEngine,
    query_embedding_cache: Arc<Mutex<QueryEmbeddingCache>>,
    local_query_last_ok_log: Option<Instant>,
    query_instruction: Option<String>,
    query_instruction_logged: bool,
    query_instruction_root: Option<PathBuf>,
}

pub type EmbeddingModel = SemanticEmbeddingModel;

/// Count-only half of [`validate_embedding_batch`]: the build path allows an
/// empty vector for a row the backend rejected, so it validates dimensions per
/// row itself and shares only this shape check.
fn validate_embedding_batch_count(
    vectors: &[Vec<f32>],
    expected_count: usize,
    context: &str,
) -> Result<(), String> {
    if expected_count > 0 && vectors.is_empty() {
        return Err(format!(
            "{context} returned no vectors for {expected_count} inputs"
        ));
    }
    if vectors.len() != expected_count {
        return Err(format!(
            "{context} returned {} vectors for {} inputs",
            vectors.len(),
            expected_count
        ));
    }
    Ok(())
}

fn validate_embedding_batch(
    vectors: &[Vec<f32>],
    expected_count: usize,
    context: &str,
) -> Result<(), String> {
    if expected_count > 0 && vectors.is_empty() {
        return Err(format!(
            "{context} returned no vectors for {expected_count} inputs"
        ));
    }

    if vectors.len() != expected_count {
        return Err(format!(
            "{context} returned {} vectors for {} inputs",
            vectors.len(),
            expected_count
        ));
    }

    let Some(first_vector) = vectors.first() else {
        return Ok(());
    };
    let expected_dimension = first_vector.len();
    validate_embedding_dimension(expected_dimension)
        .map_err(|error| format!("{context} returned {error}"))?;
    for (index, vector) in vectors.iter().enumerate() {
        if vector.len() != expected_dimension {
            return Err(format!(
                "{context} returned inconsistent embedding dimensions: vector 0 has length {expected_dimension}, vector {index} has length {}",
                vector.len()
            ));
        }
    }

    Ok(())
}

fn validate_embedding_dimension(dimension: usize) -> Result<(), String> {
    if dimension == 0 || dimension > MAX_DIMENSION {
        return Err(format!(
            "invalid embedding dimension: {dimension}; supported range is 1..={MAX_DIMENSION}"
        ));
    }

    Ok(())
}

/// Normalize a base URL: validate scheme and strip trailing slash.
/// Does NOT perform SSRF/private-IP validation — call
/// `validate_base_url_no_ssrf` separately when processing user-supplied config.
fn normalize_base_url(raw: &str) -> Result<String, String> {
    let parsed = Url::parse(raw).map_err(|error| format!("invalid base_url '{raw}': {error}"))?;
    let scheme = parsed.scheme();
    if scheme != "http" && scheme != "https" {
        return Err(format!(
            "unsupported URL scheme '{}' — only http:// and https:// are allowed",
            scheme
        ));
    }
    Ok(parsed.to_string().trim_end_matches('/').to_string())
}

/// Validate that a base URL does not point to a private/loopback address.
/// Call this on user-supplied config (at configure time) to prevent SSRF.
/// Not called for programmatically constructed configs (e.g. tests).
///
/// **Loopback is allowed.** Self-hosted embedding backends (e.g. Ollama at
/// `http://127.0.0.1:11434`) are a primary use case for `aft_search`. Loopback
/// addresses by definition cannot be exploited as SSRF targets — they only
/// reach services on the same machine. Allowing loopback unblocks Ollama at its
/// default config without opening up SSRF to LAN/intranet services, which
/// remain rejected.
///
/// **mDNS `.local` is rejected.** mDNS hostnames typically resolve to LAN
/// devices (printers, homelab servers); rejecting them before DNS lookup keeps
/// the SSRF guard meaningful for non-loopback private networks.
pub fn validate_base_url_no_ssrf(raw: &str) -> Result<(), String> {
    use std::net::{IpAddr, ToSocketAddrs};

    let parsed = Url::parse(raw).map_err(|error| format!("invalid base_url '{raw}': {error}"))?;

    let host = parsed.host_str().unwrap_or("");

    // Loopback hostnames are explicitly allowed. RFC 6761 mandates that
    // `localhost` and `*.localhost` resolve to loopback;
    // `localhost.localdomain` is a historical alias used on some Linux
    // distros. Self-hosted backends like Ollama use these by default.
    let is_loopback_host =
        host == "localhost" || host == "localhost.localdomain" || host.ends_with(".localhost");
    if is_loopback_host {
        return Ok(());
    }

    // mDNS hostnames are typically LAN devices, not loopback. Reject before
    // DNS lookup so users get a clear error rather than a private-IP error.
    if host.ends_with(".local") {
        return Err(format!(
            "base_url host '{host}' is an mDNS name — only loopback (localhost / 127.0.0.1) and public endpoints are allowed"
        ));
    }

    // Resolve the hostname. Reject private/link-local/CGNAT IPs but NOT
    // loopback (which is by definition same-machine and not an SSRF target).
    let port = parsed.port_or_known_default().unwrap_or(443);
    let addr_str = format!("{host}:{port}");
    let addrs: Vec<IpAddr> = addr_str
        .to_socket_addrs()
        .map(|iter| iter.map(|sa| sa.ip()).collect())
        .unwrap_or_default();
    for ip in &addrs {
        if is_private_non_loopback_ip(ip) {
            return Err(format!(
                "base_url '{raw}' resolves to a private/reserved IP — only loopback (127.0.0.1) and public endpoints are allowed"
            ));
        }
    }

    Ok(())
}

/// Returns true for IPv4/IPv6 addresses in private/link-local/CGNAT/benchmark/
/// multicast/reserved ranges, EXCLUDING loopback (127.0.0.0/8 and ::1). Loopback
/// is considered safe for SSRF purposes (same-machine, e.g. a local Ollama
/// endpoint) — see [`validate_base_url_no_ssrf`] for rationale.
///
/// Delegates to [`crate::url_fetch::is_private_or_reserved_ip`] so there is one
/// authoritative reserved-range list (the url_fetch copy is the maintained one;
/// this used to be a drifting subset that missed e.g. 198.18.0.0/15 and the
/// multicast/reserved blocks). We only re-add the loopback carve-out the
/// url_fetch guard deliberately does not make.
fn is_private_non_loopback_ip(ip: &std::net::IpAddr) -> bool {
    // Canonicalize so an IPv4-mapped loopback (`::ffff:127.0.0.1`) is also
    // recognized as loopback, matching the prior carve-out.
    if ip.to_canonical().is_loopback() {
        return false;
    }
    crate::url_fetch::is_private_or_reserved_ip(*ip)
}

fn build_openai_embeddings_endpoint(base_url: &str) -> String {
    if base_url.ends_with("/v1") {
        format!("{base_url}{DEFAULT_OPENAI_EMBEDDING_PATH}")
    } else {
        format!("{base_url}/v1{}", DEFAULT_OPENAI_EMBEDDING_PATH)
    }
}

fn build_ollama_embeddings_endpoint(base_url: &str) -> String {
    if base_url.ends_with("/api") {
        format!("{base_url}/embed")
    } else {
        format!("{base_url}{DEFAULT_OLLAMA_EMBEDDING_PATH}")
    }
}

fn normalize_api_key(value: Option<String>) -> Option<String> {
    value.and_then(|token| {
        let token = token.trim();
        if token.is_empty() {
            None
        } else {
            Some(token.to_string())
        }
    })
}

fn is_retryable_embedding_status(status: reqwest::StatusCode) -> bool {
    status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS
}

/// Local backends (LM Studio, Ollama, llama.cpp) can return a 4xx — usually
/// 400/409 — while a model is loading or was just unloaded. Only narrowly known
/// local-backend loading/unloaded payloads are classified transient; generic
/// 4xx bodies that merely mention phrases like "loading model" remain
/// permanent so misconfigurations do not retry forever.
fn embedding_response_body_is_transient(status: reqwest::StatusCode, raw: &str) -> bool {
    if !matches!(
        status,
        reqwest::StatusCode::BAD_REQUEST
            | reqwest::StatusCode::CONFLICT
            | reqwest::StatusCode::REQUEST_TIMEOUT
            | reqwest::StatusCode::LOCKED
            | reqwest::StatusCode::TOO_EARLY
    ) {
        return false;
    }

    let lower = raw.to_ascii_lowercase();
    let normalized = lower.trim();

    normalized.contains("model was unloaded while the request was still in queue")
        || normalized == "model is loading"
        || normalized.starts_with("model is loading,")
        || normalized.contains(r#""error":"model is loading"#)
        || normalized.contains(r#""message":"model is loading"#)
        || normalized == "model not loaded"
        || normalized.contains(r#""error":"model not loaded""#)
        || normalized.contains(r#""message":"model not loaded""#)
        || normalized == "loading model into memory"
        || normalized.contains(r#""error":"loading model into memory""#)
        || normalized.contains(r#""message":"loading model into memory""#)
        || normalized == "model is being loaded"
        || normalized.contains(r#""error":"model is being loaded""#)
        || normalized.contains(r#""message":"model is being loaded""#)
        || normalized == "model is currently loading"
        || normalized.contains(r#""error":"model is currently loading""#)
        || normalized.contains(r#""message":"model is currently loading""#)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RowTooLongDetails {
    limit_tokens: Option<usize>,
    actual_tokens: Option<usize>,
}

fn json_token_count(value: &serde_json::Value, keys: &[&str]) -> Option<usize> {
    match value {
        serde_json::Value::Object(fields) => {
            for (key, value) in fields {
                if keys
                    .iter()
                    .any(|candidate| key.eq_ignore_ascii_case(candidate))
                {
                    if let Some(count) =
                        value.as_u64().and_then(|count| usize::try_from(count).ok())
                    {
                        return Some(count);
                    }
                }
            }
            fields
                .values()
                .find_map(|value| json_token_count(value, keys))
        }
        serde_json::Value::Array(values) => values
            .iter()
            .find_map(|value| json_token_count(value, keys)),
        _ => None,
    }
}

fn number_after_phrase(value: &str, phrases: &[&str]) -> Option<usize> {
    phrases.iter().find_map(|phrase| {
        let rest = value.split_once(phrase)?.1.trim_start();
        let digits = rest
            .chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>();
        (!digits.is_empty()).then(|| digits.parse().ok()).flatten()
    })
}

/// Classify only known context-window rejections. Generic 4xx responses remain
/// permanent errors so authentication and model configuration failures abort.
fn embedding_response_row_too_long(
    status: reqwest::StatusCode,
    raw: &str,
) -> Option<RowTooLongDetails> {
    if !status.is_client_error() {
        return None;
    }

    let lower = raw.to_ascii_lowercase();
    let known_overflow = lower.contains("exceed_context_size_error")
        || lower.contains("input is too large to process")
        || lower.contains("maximum context length is")
        || lower.contains("this model's maximum context length")
        || lower.contains("input length exceeds");
    if !known_overflow {
        return None;
    }

    let parsed = serde_json::from_str::<serde_json::Value>(raw).ok();
    let limit_tokens = parsed
        .as_ref()
        .and_then(|value| json_token_count(value, &["n_ctx", "max_context_length"]))
        .or_else(|| {
            number_after_phrase(
                &lower,
                &[
                    "maximum context length is ",
                    "this model's maximum context length is ",
                    "n_ctx=",
                    "n_ctx: ",
                ],
            )
        });
    let actual_tokens = parsed
        .as_ref()
        .and_then(|value| {
            json_token_count(value, &["n_prompt_tokens", "input_tokens", "prompt_tokens"])
        })
        .or_else(|| {
            number_after_phrase(
                &lower,
                &[
                    "resulted in ",
                    "you requested ",
                    "requested ",
                    "n_prompt_tokens=",
                    "n_prompt_tokens: ",
                ],
            )
        });

    Some(RowTooLongDetails {
        limit_tokens,
        actual_tokens,
    })
}

fn is_retryable_embedding_error(error: &reqwest::Error) -> bool {
    // Retryable == transient-at-send-stage: a backend that refused, timed
    // out, or died mid-exchange deserves the same in-request retry ladder.
    embedding_send_error_is_transient(error)
}

/// Whether a send-time error means the backend is *unreachable or temporarily
/// failing* (vs. a real misconfiguration). Build requests retry both connection
/// failures and timeouts; query requests use the same classification but have a
/// one-attempt policy.
fn embedding_send_error_is_transient(error: &reqwest::Error) -> bool {
    // TLS trust failures are reported by reqwest as connect errors, but they
    // cannot recover by retrying. Check the source chain before the broad
    // connect/timeout classification so private-CA failures become terminal.
    if embedding_error_is_certificate_trust_failure(error) {
        return false;
    }
    if error.is_connect() || error.is_timeout() {
        return true;
    }
    // A connection reset/abort mid-request is the backend dying between
    // accept and response (local backends do this when they crash or restart
    // under load) — the same "temporarily failing" class as a refused
    // connection, just later in the exchange. reqwest surfaces it as a plain
    // send error. Classify from the io source chain where one exists; hyper
    // errors like IncompleteMessage ("connection closed before message
    // completed") and UnexpectedMessage ("received unexpected message from
    // connection" — the peer wrote a partial reply and closed while the
    // request was still being sent, observed on Windows CI where the socket
    // closes with unread request bytes) carry no io source, so fall back to
    // known phrases in the chain's rendered messages.
    let mut source = std::error::Error::source(error);
    while let Some(inner) = source {
        if let Some(io) = inner.downcast_ref::<std::io::Error>() {
            if matches!(
                io.kind(),
                std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::UnexpectedEof
            ) {
                return true;
            }
        }
        let rendered = inner.to_string().to_ascii_lowercase();
        if rendered.contains("connection reset")
            || rendered.contains("connection aborted")
            || rendered.contains("connection closed")
            || rendered.contains("broken pipe")
            || rendered.contains("unexpected end of file")
            || rendered.contains("unexpected message from connection")
        {
            return true;
        }
        source = std::error::Error::source(inner);
    }
    false
}

fn render_error_source_chain(error: &dyn Error) -> String {
    let mut rendered = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        rendered.push_str(": ");
        rendered.push_str(&cause.to_string());
        source = cause.source();
    }
    rendered
}

fn embedding_error_is_certificate_trust_failure(error: &reqwest::Error) -> bool {
    let rendered = render_error_source_chain(error).to_ascii_lowercase();
    [
        "unknownissuer",
        "unknown issuer",
        "invalid peer certificate",
        "certificate verify failed",
        "certificate validation failed",
        "certificate error",
    ]
    .iter()
    .any(|marker| rendered.contains(marker))
}

fn embedding_response_read_error_is_transient(error: &reqwest::Error) -> bool {
    embedding_send_error_is_transient(error) || error.is_body() || error.is_decode()
}

/// Returns the query-timeout marker for a request error when the active policy
/// is a `Query(budget)` and reqwest classifies the error as a timeout. Returns
/// an empty string otherwise — build-policy timeouts and non-timeout query
/// errors carry no marker. This is the single site that decides whether a
/// failure is "the configured query budget fired", so the fallback message can
/// name the knob (`semantic.query_timeout_ms`) without re-parsing reqwest text.
fn query_timeout_marker_for_error(
    error: &reqwest::Error,
    policy: EmbeddingRequestPolicy,
) -> String {
    match policy {
        EmbeddingRequestPolicy::Query(budget) if error.is_timeout() => {
            query_embedding_timeout_marker(budget.timeout_ms)
        }
        _ => String::new(),
    }
}

/// Stable machine marker prefixed onto embedding error strings whose root cause
/// is transient — the backend is down, timing out, or returning 5xx/429, not
/// misconfigured. The build and corpus-refresh layers key retry-vs-give-up on
/// this marker (see [`embedding_failure_is_transient`]) instead of re-parsing
/// error text, so transience stays authoritative at the one site that knows it.
/// Stripped before any user-facing display via [`strip_transient_embedding_marker`].
pub const TRANSIENT_EMBEDDING_MARKER: &str = "[transient] ";

/// True when an embedding error carries the transient marker — i.e. retrying
/// once the backend recovers is the right move, not surfacing a hard failure.
pub fn embedding_failure_is_transient(error: &str) -> bool {
    error.contains(TRANSIENT_EMBEDDING_MARKER)
}

/// Remove the machine transient marker so the message is clean for display.
pub fn strip_transient_embedding_marker(error: &str) -> String {
    error.replace(TRANSIENT_EMBEDDING_MARKER, "")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BuildTimeoutDetails {
    batch_size: usize,
    deadline_ms: u64,
    attempts: usize,
}

fn build_embedding_timeout_marker(details: BuildTimeoutDetails) -> String {
    format!(
        "{BUILD_EMBEDDING_TIMEOUT_MARKER_PREFIX}{}:{}:{}{BUILD_EMBEDDING_TIMEOUT_MARKER_SUFFIX}",
        details.batch_size, details.deadline_ms, details.attempts
    )
}

fn build_embedding_timeout_details(error: &str) -> Option<BuildTimeoutDetails> {
    let start = error.find(BUILD_EMBEDDING_TIMEOUT_MARKER_PREFIX)?
        + BUILD_EMBEDDING_TIMEOUT_MARKER_PREFIX.len();
    let end = error[start..].find(BUILD_EMBEDDING_TIMEOUT_MARKER_SUFFIX)? + start;
    let mut fields = error[start..end].split(':');
    let details = BuildTimeoutDetails {
        batch_size: fields.next()?.parse().ok()?,
        deadline_ms: fields.next()?.parse().ok()?,
        attempts: fields.next()?.parse().ok()?,
    };
    fields.next().is_none().then_some(details)
}

fn row_too_long_marker(details: RowTooLongDetails) -> String {
    format!(
        "{ROW_TOO_LONG_MARKER_PREFIX}{}:{}{ROW_TOO_LONG_MARKER_SUFFIX}",
        details.limit_tokens.unwrap_or(0),
        details.actual_tokens.unwrap_or(0),
    )
}

fn row_too_long_details(error: &str) -> Option<RowTooLongDetails> {
    let start = error.find(ROW_TOO_LONG_MARKER_PREFIX)? + ROW_TOO_LONG_MARKER_PREFIX.len();
    let end = error[start..].find(ROW_TOO_LONG_MARKER_SUFFIX)? + start;
    let mut fields = error[start..end].split(':');
    let limit_tokens = fields.next()?.parse::<usize>().ok()?;
    let actual_tokens = fields.next()?.parse::<usize>().ok()?;
    if fields.next().is_some() {
        return None;
    }
    Some(RowTooLongDetails {
        limit_tokens: (limit_tokens > 0).then_some(limit_tokens),
        actual_tokens: (actual_tokens > 0).then_some(actual_tokens),
    })
}

fn strip_row_too_long_marker(error: &str) -> String {
    let Some(details) = row_too_long_details(error) else {
        return error.to_string();
    };
    error.replace(&row_too_long_marker(details), "")
}

/// Remove only body text; the identifying name/file/kind/signature prefix is
/// retained so a shortened embedding remains attributable to its source row.
fn shrink_embed_text(text: &str, details: RowTooLongDetails) -> Option<String> {
    let body_marker = " body:";
    let body_start = text.find(body_marker)?;
    let header = &text[..body_start];
    let body = &text[body_start + body_marker.len()..];
    if body.is_empty() {
        return None;
    }

    let ratio_target = details
        .limit_tokens
        .zip(details.actual_tokens)
        .filter(|(_, actual)| *actual > 0)
        .map(|(limit, actual)| {
            (body.len() as f64 * limit as f64 / actual as f64 * ROW_SHRINK_RATIO_MARGIN) as usize
        });
    let target_bytes = ratio_target
        .unwrap_or_else(|| body.len() / 2)
        .min(body.len().saturating_sub(1));
    if target_bytes == 0 {
        return Some(header.to_string());
    }

    let shortened_body = &body[..body.floor_char_boundary(target_bytes)];
    if shortened_body.is_empty() {
        Some(header.to_string())
    } else {
        Some(format!("{header}{body_marker}{shortened_body}"))
    }
}

/// Stable machine marker prefixed onto a *query* embedding error string when
/// the failure was a request timeout — i.e. reqwest's `is_timeout()` fired
/// while running under a `Query(budget)` policy. The marker carries the budget
/// that fired (`[query-timeout:{ms}]`) so the consumer can name both the
/// mechanism and the knob (`semantic.query_timeout_ms`) without re-parsing
/// reqwest's rendered error text, which varies by backend and locale.
///
/// Classification lives here — next to the one site that knows both the policy
/// (Query with a budget) and the typed reqwest error — so it cannot drift from
/// the error shape. Stripped before user-facing display via
/// [`strip_query_embedding_timeout_marker`]; the budget is recovered via
/// [`query_embedding_timeout_budget`].
pub const QUERY_EMBEDDING_TIMEOUT_MARKER_PREFIX: &str = "[query-timeout:";
pub const QUERY_EMBEDDING_TIMEOUT_MARKER_SUFFIX: &str = "]";

/// Build the timeout marker for a given query budget. Kept here so the format
/// and the parser below stay in lockstep. `pub(crate)` so the classification
/// test in `semantic_search` can construct a marked error without duplicating
/// the format string.
pub(crate) fn query_embedding_timeout_marker(timeout_ms: u64) -> String {
    format!("{QUERY_EMBEDDING_TIMEOUT_MARKER_PREFIX}{timeout_ms}{QUERY_EMBEDDING_TIMEOUT_MARKER_SUFFIX}")
}

/// Recover the timeout budget (ms) a query embedding error carries, or `None`
/// when the failure was not a query timeout. This is the single authoritative
/// way to detect the timeout case — never substring-match on reqwest's text.
pub fn query_embedding_timeout_budget(error: &str) -> Option<u64> {
    let start = error.find(QUERY_EMBEDDING_TIMEOUT_MARKER_PREFIX)?;
    let rest = &error[start + QUERY_EMBEDDING_TIMEOUT_MARKER_PREFIX.len()..];
    let end = rest.find(QUERY_EMBEDDING_TIMEOUT_MARKER_SUFFIX)?;
    rest[..end].parse::<u64>().ok()
}

/// Remove the query-timeout marker so the message is clean for display. The
/// budget is recovered separately via [`query_embedding_timeout_budget`] before
/// stripping.
pub fn strip_query_embedding_timeout_marker(error: &str) -> String {
    if let (Some(start), Some(budget)) = (
        error.find(QUERY_EMBEDDING_TIMEOUT_MARKER_PREFIX),
        query_embedding_timeout_budget(error),
    ) {
        let marker = query_embedding_timeout_marker(budget);
        let end = start + marker.len();
        let mut cleaned = error.to_string();
        cleaned.replace_range(start..end, "");
        cleaned
    } else {
        error.to_string()
    }
}

const QUERY_EMBEDDING_BUSY_MARKER: &str = "[query-embed-busy]";

pub(crate) fn query_embedding_is_busy(error: &str) -> bool {
    error.contains(QUERY_EMBEDDING_BUSY_MARKER)
}

pub(crate) fn strip_query_embedding_busy_marker(error: &str) -> String {
    error.replace(QUERY_EMBEDDING_BUSY_MARKER, "")
}

fn sleep_before_embedding_retry(attempt_index: usize) {
    if let Some(delay_ms) = EMBEDDING_REQUEST_BACKOFF_MS.get(attempt_index) {
        std::thread::sleep(Duration::from_millis(*delay_ms));
    }
}

const QUERY_EMBEDDING_CANCELLED_MARKER: &str = "__AFT_QUERY_EMBEDDING_CANCELLED__";
const QUERY_EMBEDDING_CANCEL_POLL: Duration = Duration::from_millis(10);

enum EmbeddingExchange {
    SendFailed(reqwest::Error),
    Response {
        status: reqwest::StatusCode,
        body: Result<String, reqwest::Error>,
    },
}

fn execute_embedding_exchange(request: reqwest::blocking::RequestBuilder) -> EmbeddingExchange {
    match request.send() {
        Ok(response) => EmbeddingExchange::Response {
            status: response.status(),
            body: response.text(),
        },
        Err(error) => EmbeddingExchange::SendFailed(error),
    }
}

fn execute_query_embedding_exchange(
    request: reqwest::blocking::RequestBuilder,
) -> Result<EmbeddingExchange, String> {
    let Some(cancellation) = crate::executor::current_job_cancellation() else {
        return Ok(execute_embedding_exchange(request));
    };
    if cancellation.cancel_requested_before_commit() {
        return Err(QUERY_EMBEDDING_CANCELLED_MARKER.to_string());
    }

    let (tx, rx) = crossbeam_channel::bounded(1);
    std::thread::spawn(move || {
        let _ = tx.send(execute_embedding_exchange(request));
    });
    loop {
        match rx.try_recv() {
            Ok(exchange) => {
                if cancellation.cancel_requested_before_commit() {
                    return Err(QUERY_EMBEDDING_CANCELLED_MARKER.to_string());
                }
                return Ok(exchange);
            }
            Err(crossbeam_channel::TryRecvError::Disconnected) => {
                return Err("embedding request worker disconnected".to_string());
            }
            Err(crossbeam_channel::TryRecvError::Empty) => {}
        }
        if cancellation.wait_for_cancellation(QUERY_EMBEDDING_CANCEL_POLL) {
            return Err(QUERY_EMBEDDING_CANCELLED_MARKER.to_string());
        }
    }
}

fn send_embedding_request<F>(
    mut make_request: F,
    backend_label: &str,
    policy: EmbeddingRequestPolicy,
) -> Result<String, String>
where
    F: FnMut() -> reqwest::blocking::RequestBuilder,
{
    let max_attempts = policy.max_attempts();
    for attempt_index in 0..max_attempts {
        let last_attempt = attempt_index + 1 == max_attempts;
        let request = make_request().timeout(policy.request_timeout());

        let exchange = match policy {
            EmbeddingRequestPolicy::Build(_) => execute_embedding_exchange(request),
            EmbeddingRequestPolicy::Query(_) => execute_query_embedding_exchange(request)?,
        };
        let (status, raw) = match exchange {
            EmbeddingExchange::SendFailed(error) => {
                if let EmbeddingRequestPolicy::Build(budget) = policy {
                    if error.is_timeout() {
                        let details = BuildTimeoutDetails {
                            batch_size: budget.batch_size,
                            deadline_ms: budget.deadline_ms,
                            attempts: attempt_index + 1,
                        };
                        return Err(format!(
                            "{TRANSIENT_EMBEDDING_MARKER}{}{} request timed out: {}",
                            build_embedding_timeout_marker(details),
                            backend_label,
                            render_error_source_chain(&error),
                        ));
                    }
                }
                // A refused connection is already conclusive unreachable evidence;
                // retrying the same socket target only delays the circuit breaker.
                if error.is_connect() && embedding_send_error_is_transient(&error) {
                    return Err(format!(
                        "{TRANSIENT_EMBEDDING_MARKER}embedding backend unreachable (connection refused or connect failure): {}",
                        render_error_source_chain(&error),
                    ));
                }
                if !last_attempt && is_retryable_embedding_error(&error) {
                    sleep_before_embedding_retry(attempt_index);
                    continue;
                }
                let marker = if embedding_send_error_is_transient(&error) {
                    TRANSIENT_EMBEDDING_MARKER
                } else {
                    ""
                };
                // A query-timeout is a distinct, actionable failure: the
                // configured `semantic.query_timeout_ms` budget fired. Tag it
                // here — the only site that has both the typed reqwest error
                // and the Query budget — so the fallback can name the knob
                // without guessing at reqwest's rendered text.
                let timeout_marker = query_timeout_marker_for_error(&error, policy);
                return Err(format!(
                    "{timeout_marker}{marker}{backend_label} request failed: {}",
                    render_error_source_chain(&error)
                ));
            }
            EmbeddingExchange::Response {
                status,
                body: Ok(raw),
            } => (status, raw),
            EmbeddingExchange::Response {
                status: _,
                body: Err(error),
            } => {
                if let EmbeddingRequestPolicy::Build(budget) = policy {
                    if error.is_timeout() {
                        let details = BuildTimeoutDetails {
                            batch_size: budget.batch_size,
                            deadline_ms: budget.deadline_ms,
                            attempts: attempt_index + 1,
                        };
                        return Err(format!(
                            "{TRANSIENT_EMBEDDING_MARKER}{}{} response timed out: {}",
                            build_embedding_timeout_marker(details),
                            backend_label,
                            render_error_source_chain(&error),
                        ));
                    }
                }
                if !last_attempt && embedding_response_read_error_is_transient(&error) {
                    sleep_before_embedding_retry(attempt_index);
                    continue;
                }
                let marker = if embedding_response_read_error_is_transient(&error) {
                    TRANSIENT_EMBEDDING_MARKER
                } else {
                    ""
                };
                // A body-read timeout under a Query policy is the same budget
                // firing mid-exchange; tag it identically to the send case.
                let timeout_marker = query_timeout_marker_for_error(&error, policy);
                return Err(format!(
                    "{timeout_marker}{marker}{backend_label} response read failed: {}",
                    render_error_source_chain(&error)
                ));
            }
        };

        if status.is_success() {
            return Ok(raw);
        }

        if let Some(details) = embedding_response_row_too_long(status, &raw) {
            return Err(format!(
                "{}{} request failed (HTTP {}): {}",
                row_too_long_marker(details),
                backend_label,
                status,
                raw,
            ));
        }

        // A 4xx whose body says the model is loading/unloaded is transient on
        // local backends (LM Studio/Ollama), so treat it like a retryable
        // status: ride it out at both the in-request and build-retry layers.
        let body_transient = embedding_response_body_is_transient(status, &raw);
        if !last_attempt && (is_retryable_embedding_status(status) || body_transient) {
            sleep_before_embedding_retry(attempt_index);
            continue;
        }

        // 5xx / 429 are server-side and transient — the backend is overloaded
        // or briefly unavailable, not misconfigured. A 4xx whose body indicates
        // the model is (un)loading is also transient (local backend mid-swap).
        // Other 4xx (auth, bad request, model-not-found) is a real error the
        // user must fix; no marker.
        let marker = if is_retryable_embedding_status(status) || body_transient {
            TRANSIENT_EMBEDDING_MARKER
        } else {
            ""
        };
        return Err(format!(
            "{marker}{backend_label} request failed (HTTP {}): {}",
            status, raw
        ));
    }

    unreachable!("embedding request retries exhausted without returning")
}

fn configured_embedding_timeout_ms(config: &SemanticBackendConfig) -> u64 {
    if config.timeout_ms == 0 {
        DEFAULT_OPENAI_EMBEDDING_TIMEOUT_MS
    } else {
        config.timeout_ms
    }
}

fn query_embedding_text(query: &str, instruction: Option<&str>) -> String {
    match instruction {
        Some(task) => format!("Instruct: {task}\nQuery: {query}"),
        None => query.to_string(),
    }
}

impl SemanticEmbeddingModel {
    pub fn from_config(config: &SemanticBackendConfig) -> Result<Self, String> {
        Self::from_config_with_timeout_ms(config, configured_embedding_timeout_ms(config))
    }

    pub fn from_config_for_query(config: &SemanticBackendConfig) -> Result<Self, String> {
        // The model may later be reused by a background build, so retain the build
        // client's timeout. QueryBudget overrides each interactive HTTP request.
        Self::from_config(config)
    }

    fn from_config_with_timeout_ms(
        config: &SemanticBackendConfig,
        timeout_ms: u64,
    ) -> Result<Self, String> {
        let max_batch_size = if config.max_batch_size == 0 {
            DEFAULT_MAX_BATCH_SIZE
        } else {
            config.max_batch_size
        };

        let api_key_env = normalize_api_key(config.api_key_env.clone());
        let model = config.model.clone();

        let query_embedding_cache = Arc::new(Mutex::new(QueryEmbeddingCache::default()));
        let tls_config = crate::platform_tls::client_config()
            .map_err(|error| format!("failed to configure embedding client TLS: {error}"))?;
        let client = Client::builder()
            .timeout(Duration::from_millis(timeout_ms))
            .redirect(reqwest::redirect::Policy::none())
            .use_preconfigured_tls(tls_config)
            .build()
            .map_err(|error| format!("failed to configure embedding client: {error}"))?;

        let engine = match config.backend {
            SemanticBackend::Fastembed => {
                SemanticEmbeddingEngine::Local(LocalEmbeddingEngine::new(
                    Box::new(LocalEmbedder::new(&model)?),
                    Arc::clone(&query_embedding_cache),
                )?)
            }
            SemanticBackend::OpenAiCompatible => {
                let raw = config.base_url.as_ref().ok_or_else(|| {
                    "base_url is required for openai_compatible backend".to_string()
                })?;
                let base_url = normalize_base_url(raw)?;

                let api_key = match api_key_env {
                    Some(var_name) => Some(env::var(&var_name).map_err(|_| {
                        format!("missing api_key_env '{var_name}' for openai_compatible backend")
                    })?),
                    None => None,
                };

                SemanticEmbeddingEngine::OpenAiCompatible {
                    client,
                    model,
                    base_url,
                    api_key,
                }
            }
            SemanticBackend::Ollama => {
                let raw = config
                    .base_url
                    .as_ref()
                    .ok_or_else(|| "base_url is required for ollama backend".to_string())?;
                let base_url = normalize_base_url(raw)?;

                SemanticEmbeddingEngine::Ollama {
                    client,
                    model,
                    base_url,
                }
            }
            SemanticBackend::Synapse => SemanticEmbeddingEngine::Synapse(
                SynapseEmbeddingClient::from_config(config).map_err(|error| error.to_string())?,
            ),
        };
        let max_batch_size = match &engine {
            SemanticEmbeddingEngine::Synapse(client) => client.metadata().recommended_rows,
            _ => max_batch_size,
        };

        Ok(Self {
            backend: config.backend,
            model: config.model.clone(),
            base_url: config.base_url.clone(),
            timeout_ms,
            max_batch_size,
            adaptive_build_batch_size: max_batch_size,
            successful_build_batches_at_size: 0,
            per_item_ema_ms: None,
            dimension: None,
            engine,
            query_embedding_cache,
            local_query_last_ok_log: None,
            query_instruction: config.resolved_query_instruction().map(str::to_string),
            query_instruction_logged: false,
            query_instruction_root: config.route_project_root.clone(),
        })
    }

    #[cfg(test)]
    pub(crate) fn from_local_provider_for_test(
        provider: Box<dyn LocalEmbeddingProvider>,
        project_root: PathBuf,
    ) -> Self {
        let query_embedding_cache = Arc::new(Mutex::new(QueryEmbeddingCache::default()));
        let engine = SemanticEmbeddingEngine::Local(
            LocalEmbeddingEngine::new(provider, Arc::clone(&query_embedding_cache))
                .expect("start test local query embed worker"),
        );
        Self {
            backend: SemanticBackend::Fastembed,
            model: "test-local".to_string(),
            base_url: None,
            timeout_ms: DEFAULT_OPENAI_EMBEDDING_TIMEOUT_MS,
            max_batch_size: DEFAULT_MAX_BATCH_SIZE,
            adaptive_build_batch_size: DEFAULT_MAX_BATCH_SIZE,
            successful_build_batches_at_size: 0,
            per_item_ema_ms: None,
            dimension: None,
            engine,
            query_embedding_cache,
            local_query_last_ok_log: None,
            query_instruction: None,
            query_instruction_logged: false,
            query_instruction_root: Some(project_root),
        }
    }

    pub fn backend(&self) -> SemanticBackend {
        self.backend
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn base_url(&self) -> Option<&str> {
        self.base_url.as_deref()
    }

    pub fn max_batch_size(&self) -> usize {
        self.max_batch_size
    }

    pub fn timeout_ms(&self) -> u64 {
        self.timeout_ms
    }

    pub fn fingerprint(
        &mut self,
        config: &SemanticBackendConfig,
    ) -> Result<SemanticIndexFingerprint, String> {
        let dimension = self.dimension()?;
        let mut fingerprint = SemanticIndexFingerprint::from_config(config, dimension);
        if let SemanticEmbeddingEngine::Synapse(client) = &self.engine {
            let identity = client.identity();
            fingerprint.synapse_fingerprint = Some(identity.fingerprint.clone());
            fingerprint.synapse_table_epoch = Some(identity.table_epoch);
            fingerprint.synapse_equivalent_to = identity.equivalent_to.clone();
        }
        Ok(fingerprint)
    }

    fn uses_http_embedding_backend(&self) -> bool {
        matches!(
            &self.engine,
            SemanticEmbeddingEngine::OpenAiCompatible { .. }
                | SemanticEmbeddingEngine::Ollama { .. }
        )
    }

    fn build_request_deadline_ms(&self, batch_size: usize) -> u64 {
        let batch_size = batch_size.max(1);
        match self.per_item_ema_ms {
            Some(per_item_ms) => {
                let scaled =
                    (per_item_ms * batch_size as f64 * BUILD_PER_ITEM_SAFETY_FACTOR).ceil();
                let scaled = if scaled.is_finite() {
                    scaled.min(u64::MAX as f64) as u64
                } else {
                    u64::MAX
                };
                self.timeout_ms.max(scaled)
            }
            None => self.timeout_ms.max(
                self.timeout_ms
                    .saturating_mul(batch_size as u64)
                    .div_ceil(BUILD_INITIAL_BATCH_DIVISOR),
            ),
        }
    }

    fn build_request_budget(&self, batch_size: usize) -> BuildRequestBudget {
        BuildRequestBudget {
            batch_size,
            deadline_ms: self.build_request_deadline_ms(batch_size),
        }
    }

    fn note_successful_build_batch(&mut self, batch_size: usize, elapsed: Duration) {
        let measured_per_item_ms = elapsed.as_secs_f64() * 1_000.0 / batch_size.max(1) as f64;
        self.per_item_ema_ms = Some(match self.per_item_ema_ms {
            Some(previous) => {
                previous * (1.0 - BUILD_PER_ITEM_EMA_ALPHA)
                    + measured_per_item_ms * BUILD_PER_ITEM_EMA_ALPHA
            }
            None => measured_per_item_ms,
        });

        if batch_size != self.adaptive_build_batch_size {
            return;
        }
        self.successful_build_batches_at_size =
            self.successful_build_batches_at_size.saturating_add(1);
        if self.successful_build_batches_at_size < BUILD_BATCH_GROWTH_SUCCESSES
            || self.adaptive_build_batch_size >= self.max_batch_size
        {
            return;
        }

        let old_size = self.adaptive_build_batch_size;
        self.adaptive_build_batch_size = old_size.saturating_mul(2).min(self.max_batch_size);
        self.successful_build_batches_at_size = 0;
        slog_info!(
            "semantic embed batch size {} -> {} after successful batches (per_item_ms={:.0})",
            old_size,
            self.adaptive_build_batch_size,
            self.per_item_ema_ms.unwrap_or(measured_per_item_ms),
        );
    }

    fn embed_http_batch_overflow_resilient(
        &mut self,
        texts: Vec<String>,
    ) -> Result<Vec<AdaptiveBuildRow>, String> {
        let budget = self.build_request_budget(texts.len());
        match self.embed_texts(texts.clone(), EmbeddingRequestPolicy::Build(budget)) {
            Ok(vectors) => {
                validate_embedding_batch(&vectors, texts.len(), "embedding backend")?;
                Ok(texts
                    .into_iter()
                    .zip(vectors)
                    .map(|(embedded_text, vector)| AdaptiveBuildRow {
                        metadata: BuildEmbeddingRowMetadata {
                            embedded_text,
                            skipped_reason: None,
                        },
                        vector: Some(vector),
                    })
                    .collect())
            }
            Err(error) => {
                let Some(details) = row_too_long_details(&error) else {
                    return Err(error);
                };

                if texts.len() > 1 {
                    let right = texts.len().div_ceil(2);
                    let mut left_rows =
                        self.embed_http_batch_overflow_resilient(texts[..right].to_vec())?;
                    let mut right_rows =
                        self.embed_http_batch_overflow_resilient(texts[right..].to_vec())?;
                    left_rows.append(&mut right_rows);
                    return Ok(left_rows);
                }

                let mut embedded_text = texts
                    .into_iter()
                    .next()
                    .expect("overflow response had at least one input");
                let mut latest_details = details;
                let mut latest_error = error;
                for _ in 0..MAX_ROW_SHRINK_ATTEMPTS {
                    let Some(shortened) = shrink_embed_text(&embedded_text, latest_details) else {
                        break;
                    };
                    embedded_text = shortened;
                    let budget = self.build_request_budget(1);
                    match self.embed_texts(
                        vec![embedded_text.clone()],
                        EmbeddingRequestPolicy::Build(budget),
                    ) {
                        Ok(mut vectors) => {
                            validate_embedding_batch(&vectors, 1, "embedding backend")?;
                            return Ok(vec![AdaptiveBuildRow {
                                metadata: BuildEmbeddingRowMetadata {
                                    embedded_text,
                                    skipped_reason: None,
                                },
                                vector: Some(vectors.remove(0)),
                            }]);
                        }
                        Err(error) => {
                            let Some(details) = row_too_long_details(&error) else {
                                return Err(error);
                            };
                            latest_details = details;
                            latest_error = error;
                        }
                    }
                }

                Ok(vec![AdaptiveBuildRow {
                    metadata: BuildEmbeddingRowMetadata {
                        embedded_text,
                        skipped_reason: Some(strip_row_too_long_marker(&latest_error)),
                    },
                    vector: None,
                }])
            }
        }
    }

    fn embed_build_http_adaptive(&mut self, texts: Vec<String>) -> Result<Vec<Vec<f32>>, String> {
        clear_http_build_metadata();
        let mut rows = Vec::with_capacity(texts.len());
        let mut cursor = 0usize;

        while cursor < texts.len() {
            let batch_size = self
                .adaptive_build_batch_size
                .max(1)
                .min(texts.len() - cursor);
            let batch = texts[cursor..cursor + batch_size].to_vec();
            let started = Instant::now();
            match self.embed_http_batch_overflow_resilient(batch) {
                Ok(mut batch_rows) => {
                    self.note_successful_build_batch(batch_size, started.elapsed());
                    rows.append(&mut batch_rows);
                    cursor += batch_size;
                }
                Err(error) => {
                    let Some(timeout) = build_embedding_timeout_details(&error) else {
                        return Err(error);
                    };
                    if timeout.batch_size == 1 {
                        return Err(format!(
                            "{TRANSIENT_EMBEDDING_MARKER}single-item request timed out at {} ms: treating as down ({} attempt(s))",
                            self.timeout_ms, timeout.attempts,
                        ));
                    }

                    let new_size = timeout.batch_size.div_ceil(2).max(1);
                    self.adaptive_build_batch_size = new_size;
                    self.successful_build_batches_at_size = 0;
                    let per_item_ms = self
                        .per_item_ema_ms
                        .unwrap_or(self.timeout_ms as f64 / BUILD_INITIAL_BATCH_DIVISOR as f64);
                    slog_info!(
                        "semantic embed batch size {} -> {} after timeout (per_item_ms={:.0}, deadline_ms={})",
                        timeout.batch_size,
                        new_size,
                        per_item_ms,
                        timeout.deadline_ms,
                    );
                }
            }
        }

        let mut metadata = Vec::with_capacity(rows.len());
        let mut vectors = Vec::with_capacity(rows.len());
        for row in rows {
            metadata.push(row.metadata);
            vectors.push(row.vector.unwrap_or_default());
        }
        set_http_build_metadata(metadata);
        Ok(vectors)
    }

    pub fn dimension(&mut self) -> Result<usize, String> {
        if let Some(dimension) = self.dimension {
            return Ok(dimension);
        }

        let dimension = if self.uses_http_embedding_backend() {
            let vectors = self.embed(vec!["semantic index fingerprint probe".to_string()])?;
            vectors
                .first()
                .map(|v| v.len())
                .ok_or_else(|| "embedding backend returned no vectors".to_string())?
        } else {
            match &mut self.engine {
                SemanticEmbeddingEngine::Local(engine) => {
                    let vectors = engine
                        .model
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .embed(&["semantic index fingerprint probe".to_string()])?;
                    vectors
                        .first()
                        .map(|v| v.len())
                        .ok_or_else(|| "embedding backend returned no vectors".to_string())?
                }
                SemanticEmbeddingEngine::Synapse(client) => client
                    .probe_dimension(Duration::from_millis(self.timeout_ms))
                    .map_err(|error| error.to_string())?,
                SemanticEmbeddingEngine::OpenAiCompatible { .. }
                | SemanticEmbeddingEngine::Ollama { .. } => {
                    unreachable!("HTTP backends are handled above")
                }
            }
        };

        self.dimension = Some(dimension);
        Ok(dimension)
    }

    pub fn embed(&mut self, texts: Vec<String>) -> Result<Vec<Vec<f32>>, String> {
        if self.uses_http_embedding_backend() {
            self.embed_build_http_adaptive(texts)
        } else {
            let budget = self.build_request_budget(texts.len());
            self.embed_texts(texts, EmbeddingRequestPolicy::Build(budget))
        }
    }

    pub fn embed_query_cached(
        &mut self,
        query: &str,
        budget: QueryBudget,
    ) -> Result<Vec<f32>, String> {
        if !self.query_instruction_logged {
            let root = self
                .query_instruction_root
                .as_deref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "<unscoped>".to_string());
            match self.query_instruction.as_deref() {
                Some(instruction) => slog_info!(
                    "semantic query instruction for root {} model {}: {:?}",
                    root,
                    self.model,
                    instruction
                ),
                None => slog_info!(
                    "semantic query instruction for root {} model {}: none",
                    root,
                    self.model
                ),
            }
            self.query_instruction_logged = true;
        }
        let query_text = query_embedding_text(query, self.query_instruction.as_deref());
        self.embed_texts(vec![query_text], EmbeddingRequestPolicy::Query(budget))?
            .into_iter()
            .next()
            .ok_or_else(|| "embedding model returned no query vector".to_string())
    }

    pub fn query_embedding_cache_stats(&self) -> (u64, u64, usize) {
        let cache = self
            .query_embedding_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (cache.hits, cache.misses, cache.query_embedding_cache.len())
    }

    fn log_local_query_embed(&mut self, elapsed: Duration, budget: QueryBudget, outcome: &str) {
        let elapsed_ms = elapsed.as_millis().min(u128::from(u64::MAX)) as u64;
        if outcome != "busy" {
            record_query_embed_observation(
                self.query_instruction_root.as_deref(),
                elapsed_ms,
                outcome == "timeout",
            );
        }
        let should_log = if outcome == "ok" {
            self.local_query_last_ok_log
                .is_none_or(|last| last.elapsed() >= QUERY_EMBED_OK_LOG_INTERVAL)
        } else {
            true
        };
        if !should_log {
            return;
        }
        if outcome == "ok" {
            self.local_query_last_ok_log = Some(Instant::now());
        }
        slog_info!(
            "semantic query embed: backend=fastembed model={} elapsed_ms={} budget_ms={} outcome={}",
            self.model,
            elapsed_ms,
            budget.timeout_ms,
            outcome,
        );
    }

    fn embed_local_query(
        &mut self,
        texts: Vec<String>,
        cache_key: String,
        budget: QueryBudget,
    ) -> Result<Vec<Vec<f32>>, String> {
        let started = Instant::now();
        let receiver = match &self.engine {
            SemanticEmbeddingEngine::Local(engine) => {
                engine.query_worker.try_submit(texts, cache_key)
            }
            _ => unreachable!("local query path requires the local embedding engine"),
        };
        let Some(receiver) = receiver else {
            self.log_local_query_embed(started.elapsed(), budget, "busy");
            return Err(format!(
                "{QUERY_EMBEDDING_BUSY_MARKER}fastembed query embedder is busy finishing an earlier inference"
            ));
        };
        match receiver.recv_timeout(Duration::from_millis(budget.timeout_ms)) {
            Ok(result) => {
                self.log_local_query_embed(started.elapsed(), budget, "ok");
                result
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                self.log_local_query_embed(started.elapsed(), budget, "timeout");
                Err(format!(
                    "{}{TRANSIENT_EMBEDDING_MARKER}fastembed query embedding timed out after {}ms",
                    query_embedding_timeout_marker(budget.timeout_ms),
                    budget.timeout_ms,
                ))
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                self.log_local_query_embed(started.elapsed(), budget, "busy");
                Err(format!(
                    "{QUERY_EMBEDDING_BUSY_MARKER}fastembed query embed worker disconnected"
                ))
            }
        }
    }

    fn embed_texts(
        &mut self,
        texts: Vec<String>,
        policy: EmbeddingRequestPolicy,
    ) -> Result<Vec<Vec<f32>>, String> {
        let query_cache_key = match policy {
            EmbeddingRequestPolicy::Build(_) => None,
            EmbeddingRequestPolicy::Query(_) => texts.first().cloned(),
        };
        let cached_vectors = query_cache_key.as_ref().and_then(|query| {
            let mut cache = self
                .query_embedding_cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let vector = cache.query_embedding_cache.get(query).cloned();
            if vector.is_some() {
                cache.hits = cache.hits.saturating_add(1);
            } else {
                cache.misses = cache.misses.saturating_add(1);
            }
            vector.map(|vector| vec![vector])
        });
        let cache_hit = u64::from(cached_vectors.is_some());
        let requested = if query_cache_key.is_some() && cached_vectors.is_none() {
            texts.len() as u64
        } else {
            0
        };
        let local_query_result = if cached_vectors.is_none() {
            match policy {
                EmbeddingRequestPolicy::Query(budget)
                    if matches!(&self.engine, SemanticEmbeddingEngine::Local(_)) =>
                {
                    Some(
                        self.embed_local_query(
                            texts.clone(),
                            query_cache_key
                                .clone()
                                .expect("query policy has a cache key"),
                            budget,
                        ),
                    )
                }
                _ => None,
            }
        } else {
            None
        };
        let local_worker_busy = local_query_result.as_ref().is_some_and(|result| {
            result
                .as_ref()
                .is_err_and(|error| query_embedding_is_busy(error))
        });
        let live_calls =
            u64::from(requested > 0 && self.is_live_query_provider() && !local_worker_busy);
        crate::search_b2::embed_counter::record(crate::search_b2::embed_counter::EmbedCounts {
            requested,
            cache_hits: cache_hit,
            live_calls,
        });
        if let Some(vectors) = cached_vectors {
            return Ok(vectors);
        }
        if let Some(result) = local_query_result {
            return result;
        }

        let result = match &mut self.engine {
            SemanticEmbeddingEngine::Local(engine) => engine
                .model
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .embed(&texts)
                .map_err(|error| format!("failed to embed batch: {error}")),
            SemanticEmbeddingEngine::OpenAiCompatible {
                client,
                model,
                base_url,
                api_key,
            } => {
                let expected_text_count = texts.len();
                let endpoint = build_openai_embeddings_endpoint(base_url);
                let body = serde_json::json!({
                    "input": texts,
                    "model": model,
                });

                let raw = send_embedding_request(
                    || {
                        // `.json(&body)` sets Content-Type: application/json
                        // automatically. Do NOT add `.header("Content-Type",
                        // "application/json")` afterwards — RequestBuilder::header()
                        // calls HeaderMap::append, which produces TWO Content-Type
                        // headers on the wire. OpenAI's /v1/embeddings endpoint
                        // treats duplicate Content-Type as malformed and rejects
                        // the body with 400 "you must provide a model parameter"
                        // even when `model` is set. Verified end-to-end against
                        // api.openai.com. See issue #36.
                        let mut request = client.post(&endpoint).json(&body);

                        if let Some(api_key) = api_key {
                            request = request.header("Authorization", format!("Bearer {api_key}"));
                        }

                        request
                    },
                    "openai compatible",
                    policy,
                )?;

                #[derive(Deserialize)]
                struct OpenAiResponse {
                    data: Vec<OpenAiEmbeddingResult>,
                }

                #[derive(Deserialize)]
                struct OpenAiEmbeddingResult {
                    embedding: Vec<f32>,
                    index: Option<u32>,
                }

                let parsed: OpenAiResponse = serde_json::from_str(&raw)
                    .map_err(|error| format!("invalid openai compatible response: {error}"))?;
                if parsed.data.len() != expected_text_count {
                    return Err(format!(
                        "openai compatible response returned {} embeddings for {} inputs",
                        parsed.data.len(),
                        expected_text_count
                    ));
                }

                let mut vectors = vec![Vec::new(); parsed.data.len()];
                for (i, item) in parsed.data.into_iter().enumerate() {
                    let index = item.index.unwrap_or(i as u32) as usize;
                    if index >= vectors.len() {
                        return Err(
                            "openai compatible response contains invalid vector index".to_string()
                        );
                    }
                    vectors[index] = item.embedding;
                }

                for vector in &vectors {
                    if vector.is_empty() {
                        return Err(
                            "openai compatible response contained missing vectors".to_string()
                        );
                    }
                }

                self.dimension = vectors.first().map(Vec::len);
                Ok(vectors)
            }
            SemanticEmbeddingEngine::Ollama {
                client,
                model,
                base_url,
            } => {
                let expected_text_count = texts.len();
                let endpoint = build_ollama_embeddings_endpoint(base_url);

                #[derive(Serialize)]
                struct OllamaPayload<'a> {
                    model: &'a str,
                    input: Vec<String>,
                }

                let payload = OllamaPayload {
                    model,
                    input: texts,
                };

                let raw = send_embedding_request(
                    || {
                        // `.json(&payload)` sets Content-Type automatically.
                        // Same duplicate-header trap as the OpenAI branch above
                        // — most Ollama servers tolerate it, but the
                        // single-Content-Type form is the correct one.
                        client.post(&endpoint).json(&payload)
                    },
                    "ollama",
                    policy,
                )?;

                #[derive(Deserialize)]
                struct OllamaResponse {
                    embeddings: Vec<Vec<f32>>,
                }

                let parsed: OllamaResponse = serde_json::from_str(&raw)
                    .map_err(|error| format!("invalid ollama response: {error}"))?;
                if parsed.embeddings.is_empty() {
                    return Err("ollama response returned no embeddings".to_string());
                }
                if parsed.embeddings.len() != expected_text_count {
                    return Err(format!(
                        "ollama response returned {} embeddings for {} inputs",
                        parsed.embeddings.len(),
                        expected_text_count
                    ));
                }

                let vectors = parsed.embeddings;
                for vector in &vectors {
                    if vector.is_empty() {
                        return Err("ollama response contained empty embeddings".to_string());
                    }
                }

                self.dimension = vectors.first().map(Vec::len);
                Ok(vectors)
            }
            SemanticEmbeddingEngine::Synapse(client) => {
                let vectors = match policy {
                    EmbeddingRequestPolicy::Build(_) => client
                        .embed_batch(&texts)
                        .map_err(|error| error.to_string())?,
                    EmbeddingRequestPolicy::Query(budget) => {
                        let timeout = Duration::from_millis(budget.timeout_ms);
                        texts
                            .iter()
                            .map(|text| client.embed_query(text, timeout))
                            .collect::<Result<Vec<_>, _>>()
                            .map_err(|error| error.to_string())?
                    }
                };
                self.dimension = vectors.first().map(Vec::len);
                Ok(vectors)
            }
        };

        if let (Some(query), Ok(vectors)) = (query_cache_key, &result) {
            if let Some(vector) = vectors.first() {
                self.query_embedding_cache
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(query, vector.clone());
            }
        }

        result
    }

    fn is_live_query_provider(&self) -> bool {
        match &self.engine {
            // The offline fixture serves checked-in vectors over HTTP, so crossing
            // that socket is observable work but not a live model invocation.
            SemanticEmbeddingEngine::OpenAiCompatible { model, .. } => {
                model != crate::search_b2::embed_counter::FIXTURE_PROVIDER_MODEL
            }
            SemanticEmbeddingEngine::Local(_)
            | SemanticEmbeddingEngine::Ollama { .. }
            | SemanticEmbeddingEngine::Synapse(_) => true,
        }
    }
}

/// Platform library filename for the plugin-managed ONNX Runtime.
///
/// Mirrors `ORT_PLATFORM_MAP` in packages/aft-bridge/src/onnx-runtime.ts. A
/// layout change on either side must update both — the plugin downloads the
/// runtime into `<storage_dir>/onnxruntime/<version>/` and this resolver must
/// find it at the same path.
#[cfg(target_os = "linux")]
const MANAGED_ORT_LIB_NAME: &str = "libonnxruntime.so";
#[cfg(target_os = "macos")]
const MANAGED_ORT_LIB_NAME: &str = "libonnxruntime.dylib";
#[cfg(target_os = "windows")]
const MANAGED_ORT_LIB_NAME: &str = "onnxruntime.dll";

/// Minimum managed ONNX Runtime minor version this resolver will accept.
///
/// Mirrors the `REQUIRED_ORT_MIN_MINOR` floor in onnx-runtime.ts and the 1.20
/// floor `pre_validate_onnx_runtime` enforces. A managed install below this
/// would be handed to ort and rejected there, so the resolver must skip it.
const MANAGED_ORT_MIN_MINOR: u32 = 20;

/// Resolve the plugin-managed ONNX Runtime under the ACTIVE storage dir and
/// export it as `ORT_DYLIB_PATH` for the process.
///
/// The plugin (packages/aft-bridge/src/onnx-runtime.ts) downloads the runtime
/// to `<storage_dir>/onnxruntime/<version>/<libname>` and exports ORT_DYLIB_PATH
/// into the child env. A bare `aft` binary has no such step: without this
/// resolver, `pre_validate_onnx_runtime` dlopens the bare soname, which only
/// works with a system-installed runtime. This makes the standalone binary pick
/// up the runtime the plugin already downloaded.
///
/// Resolution order:
///   1. If `ORT_DYLIB_PATH` is non-empty (an explicit user override, or the
///      plugin already exported it), do nothing — the caller's choice wins and
///      the resolver must not run at all.
///   2. Enumerate `<storage_dir>/onnxruntime/` version directories, keep only
///      parseable `1.x.y` with x >= 20, pick the highest, and if its library
///      file exists set `ORT_DYLIB_PATH` to it.
///   3. Otherwise leave the env untouched; `pre_validate_onnx_runtime` falls
///      back to the bare soname + doctor hint as before.
///
/// # Process-global env mutation
/// This sets a process-wide env var and must run ONCE at startup, before any
/// worker threads spawn (the warmup CLI main and the standalone main's semantic
/// init path). Setting it lazily from a worker thread would race ort's own
/// dlopen and other threads reading the env. The function is idempotent: once
/// `ORT_DYLIB_PATH` is set, subsequent calls short-circuit.
pub fn resolve_managed_onnx_runtime(storage_dir: &Path) {
    if onnx_runtime_override_configured_with(|name| std::env::var_os(name)) {
        return;
    }
    let Some(lib_path) = find_managed_onnx_runtime(storage_dir) else {
        return;
    };
    std::env::set_var("ORT_DYLIB_PATH", &lib_path);
    slog_info!(
        "using plugin-managed ONNX Runtime at {}",
        lib_path.display()
    );
}

fn onnx_runtime_override_configured_with(
    lookup: impl FnOnce(&str) -> Option<std::ffi::OsString>,
) -> bool {
    lookup("ORT_DYLIB_PATH").is_some_and(|value| !value.is_empty())
}

/// Find the highest compatible managed ONNX Runtime library under
/// `<storage_dir>/onnxruntime/`, or None when absent/incompatible.
///
/// Mirrors the plugin's `resolveCachedOnnxRuntimeDir`: the library may live at
/// the version root (the plugin's own flattened install) or under a `lib/`
/// subdir (manual Microsoft-archive installs, issue #71).
fn find_managed_onnx_runtime(storage_dir: &Path) -> Option<PathBuf> {
    let base = storage_dir.join("onnxruntime");
    let entries = std::fs::read_dir(&base).ok()?;
    #[cfg(test)]
    {
        // Test-only probe: counts how many times the resolver actually reads
        // the storage tree. Lets a negative-control test assert that a pre-set
        // ORT_DYLIB_PATH short-circuits the resolver without touching the tree.
        MANAGED_ORT_PROBE_READS.fetch_add(1, Ordering::Relaxed);
    }
    let mut best: Option<(u32, u32, PathBuf)> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some((major, minor)) = parse_managed_ort_version(&entry.file_name().to_string_lossy())
        else {
            continue;
        };
        if major != 1 || minor < MANAGED_ORT_MIN_MINOR {
            continue;
        }
        let Some(lib_path) = managed_ort_lib_in_version_dir(&path) else {
            continue;
        };
        if best
            .as_ref()
            .is_none_or(|(best_major, best_minor, _)| (major, minor) > (*best_major, *best_minor))
        {
            best = Some((major, minor, lib_path));
        }
    }
    best.map(|(_, _, path)| path)
}

/// Locate the library file inside one `<version>` directory, preferring the
/// version root over a `lib/` subdir (mirrors `resolveCachedOnnxRuntimeDir`).
fn managed_ort_lib_in_version_dir(version_dir: &Path) -> Option<PathBuf> {
    let root = version_dir.join(MANAGED_ORT_LIB_NAME);
    if root.is_file() {
        return Some(root);
    }
    let lib_subdir = version_dir.join("lib").join(MANAGED_ORT_LIB_NAME);
    if lib_subdir.is_file() {
        return Some(lib_subdir);
    }
    None
}

/// Parse a `major.minor.patch` triple from a version directory name. Returns
/// None for anything that is not exactly a three-part numeric version (so
/// non-version dirs and malformed names are ignored).
fn parse_managed_ort_version(name: &str) -> Option<(u32, u32)> {
    let mut parts = name.split('.');
    let major = parts.next()?.parse::<u32>().ok()?;
    let minor = parts.next()?.parse::<u32>().ok()?;
    let _patch = parts.next()?.parse::<u32>().ok()?;
    // Reject trailing junk like "1.24.4.tmp" or "1.24.4.5".
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor))
}

/// Pre-validate ONNX Runtime by attempting a raw dlopen before ort touches it.
/// This catches broken/incompatible .so files without risking a panic in the ort crate.
/// Also checks the runtime version via OrtGetApiBase if available.
pub fn pre_validate_onnx_runtime() -> Result<(), String> {
    let dylib_path = std::env::var("ORT_DYLIB_PATH").ok();

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        #[cfg(target_os = "linux")]
        let default_name = "libonnxruntime.so";
        #[cfg(target_os = "macos")]
        let default_name = "libonnxruntime.dylib";

        let lib_name = dylib_path.as_deref().unwrap_or(default_name);

        unsafe {
            let c_name = std::ffi::CString::new(lib_name)
                .map_err(|e| format!("invalid library path: {}", e))?;
            let handle = libc::dlopen(c_name.as_ptr(), libc::RTLD_NOW);
            if handle.is_null() {
                let err = libc::dlerror();
                let msg = if err.is_null() {
                    "unknown dlopen error".to_string()
                } else {
                    std::ffi::CStr::from_ptr(err).to_string_lossy().into_owned()
                };
                return Err(format!(
                    "{ONNX_RUNTIME_MISSING_PREFIX} dlopen('{}') failed: {}. \
                     Run `npx @cortexkit/aft doctor --fix` to install it.",
                    lib_name, msg
                ));
            }

            // Try to detect the runtime version from the actual loaded library
            // path first. A bare dlopen("libonnxruntime.so") may resolve to an
            // older system ORT through loader search paths; checking only the
            // caller-supplied soname would miss that and let ort fail opaquely.
            let (detected_version, version_source) =
                detect_ort_version_from_loaded_library(handle, lib_name);

            libc::dlclose(handle);

            // Check version compatibility — we need 1.20+.
            if let Some(ref version) = detected_version {
                let parts: Vec<&str> = version.split('.').collect();
                if let (Some(major), Some(minor)) = (
                    parts.first().and_then(|s| s.parse::<u32>().ok()),
                    parts.get(1).and_then(|s| s.parse::<u32>().ok()),
                ) {
                    if major != 1 || minor < 20 {
                        return Err(format_ort_version_mismatch(version, &version_source));
                    }
                }
            }
        }
    }

    #[cfg(target_os = "windows")]
    {
        // Validate ONNX Runtime availability on Windows by loading the DLL
        // via LoadLibraryExW before the ort crate attempts its own LoadLibrary.
        // This way we can produce a friendly error (with installation hints)
        // instead of a raw LoadLibrary failure from deep inside fastembed.
        let lib_name = dylib_path.as_deref().unwrap_or("onnxruntime.dll");

        // Use kernel32 LoadLibraryExW for the validation — built-in, no
        // crate dependency required. GetModuleFileNameW resolves the loaded
        // DLL path for version probing via the version.dll API.
        #[link(name = "kernel32")]
        extern "system" {
            fn LoadLibraryExW(
                lpLibFileName: *const u16,
                hFile: *mut std::ffi::c_void,
                dwFlags: u32,
            ) -> *mut std::ffi::c_void;
            fn FreeLibrary(hLibModule: *mut std::ffi::c_void) -> i32;
            fn GetModuleFileNameW(
                hModule: *mut std::ffi::c_void,
                lpFilename: *mut u16,
                nSize: u32,
            ) -> u32;
        }

        #[link(name = "version")]
        extern "system" {
            fn GetFileVersionInfoSizeW(lptstrFilename: *const u16, lpdwHandle: *mut u32) -> u32;
            fn GetFileVersionInfoW(
                lptstrFilename: *const u16,
                dwHandle: u32,
                dwLen: u32,
                lpData: *mut std::ffi::c_void,
            ) -> i32;
            fn VerQueryValueW(
                pBlock: *mut std::ffi::c_void,
                lpSubBlock: *const u16,
                lplpBuffer: *mut *mut std::ffi::c_void,
                puLen: *mut u32,
            ) -> i32;
        }

        #[repr(C)]
        struct VS_FIXEDFILEINFO {
            dw_signature: u32,
            dw_struc_version: u32,
            dw_file_version_ms: u32, // HIWORD major, LOWORD minor
            dw_file_version_ls: u32, // HIWORD build, LOWORD revision
            dw_product_version_ms: u32,
            dw_product_version_ls: u32,
            dw_file_flags_mask: u32,
            dw_file_flags: u32,
            dw_file_os: u32,
            dw_file_type: u32,
            dw_file_subtype: u32,
            dw_file_date_ms: u32,
            dw_file_date_ls: u32,
        }

        unsafe {
            use std::os::windows::ffi::OsStrExt;
            let wide: Vec<u16> = std::ffi::OsStr::new(lib_name)
                .encode_wide()
                .chain(std::iter::once(0))
                .collect();

            let handle = LoadLibraryExW(wide.as_ptr(), std::ptr::null_mut(), 0);
            if handle.is_null() {
                let err = std::io::Error::last_os_error();
                return Err(format!(
                    "{ONNX_RUNTIME_MISSING_PREFIX} LoadLibraryExW('{}') failed: {}. \
                     Run `npx @cortexkit/aft doctor --fix` to install it.",
                    lib_name, err
                ));
            }

            // Probe the file version from PE resources so we can reject
            // outdated DLLs (e.g. v1.9.x) before the ort crate panics.
            let mut detected_major: u32 = 0;
            let mut detected_minor: u32 = 0;
            // Use MAX_UNICODEPATH (32767) so deeply nested ORT paths (e.g.
            // long NuGet package paths under %USERPROFILE%) never truncate.
            // GetModuleFileNameW truncates silently when the buffer is too
            // small, which causes version probing to fail and the version
            // check to be bypassed — better to allocate generously.
            let mut path_buf = [0u16; 32767];
            let path_len = GetModuleFileNameW(handle, path_buf.as_mut_ptr(), 32767);
            if path_len > 0 {
                let mut dummy_handle: u32 = 0;
                let info_size = GetFileVersionInfoSizeW(path_buf.as_ptr(), &mut dummy_handle);
                if info_size > 0 {
                    let mut info = vec![0u8; info_size as usize];
                    if GetFileVersionInfoW(
                        path_buf.as_ptr(),
                        0,
                        info_size,
                        info.as_mut_ptr() as *mut std::ffi::c_void,
                    ) != 0
                    {
                        let sub_block = "\\\0".encode_utf16().collect::<Vec<u16>>();
                        let mut vs_info: *mut std::ffi::c_void = std::ptr::null_mut();
                        let mut vs_len: u32 = 0;
                        if VerQueryValueW(
                            info.as_mut_ptr() as *mut std::ffi::c_void,
                            sub_block.as_ptr(),
                            &mut vs_info,
                            &mut vs_len,
                        ) != 0
                            && !vs_info.is_null()
                        {
                            let fixed = vs_info as *const VS_FIXEDFILEINFO;
                            detected_major = (*fixed).dw_file_version_ms >> 16;
                            detected_minor = (*fixed).dw_file_version_ms & 0xFFFF;
                        }
                    }
                }
            }

            FreeLibrary(handle);

            // Version compatibility check (mirrors the Linux/macOS path).
            // If version could not be detected (detected_major == 0) we let
            // the load succeed — the ort crate will diagnose further.
            if detected_major != 0 && (detected_major != 1 || detected_minor < 20) {
                let ver = format!("{}.{}", detected_major, detected_minor);
                return Err(format_ort_version_mismatch(&ver, lib_name));
            }
        }
    }

    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
unsafe fn loaded_library_path_from_handle(handle: *mut std::ffi::c_void) -> Option<String> {
    let symbol_name = std::ffi::CString::new("OrtGetApiBase").ok()?;
    let symbol = unsafe { libc::dlsym(handle, symbol_name.as_ptr()) };
    if symbol.is_null() {
        return None;
    }

    let mut info = std::mem::MaybeUninit::<libc::Dl_info>::uninit();
    if unsafe { libc::dladdr(symbol, info.as_mut_ptr()) } == 0 {
        return None;
    }

    let info = unsafe { info.assume_init() };
    if info.dli_fname.is_null() {
        return None;
    }

    Some(
        unsafe { std::ffi::CStr::from_ptr(info.dli_fname) }
            .to_string_lossy()
            .into_owned(),
    )
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn detect_ort_version_from_resolved_or_requested(
    resolved_path: Option<String>,
    requested_lib_name: &str,
) -> (Option<String>, String) {
    if let Some(path) = resolved_path {
        if let Some(version) = detect_ort_version_from_path(&path) {
            return (Some(version), path);
        }
        return (detect_ort_version_from_path(requested_lib_name), path);
    }

    (
        detect_ort_version_from_path(requested_lib_name),
        requested_lib_name.to_string(),
    )
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn detect_ort_version_from_loaded_library(
    handle: *mut std::ffi::c_void,
    requested_lib_name: &str,
) -> (Option<String>, String) {
    detect_ort_version_from_resolved_or_requested(
        unsafe { loaded_library_path_from_handle(handle) },
        requested_lib_name,
    )
}

/// Try to extract the ORT version from the library filename or resolved symlink.
/// Examples: "libonnxruntime.so.1.19.0" → "1.19.0", "libonnxruntime.1.24.4.dylib" → "1.24.4"
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn detect_ort_version_from_path(lib_path: &str) -> Option<String> {
    let path = std::path::Path::new(lib_path);

    // Try the path as given, then follow symlinks
    for candidate in [Some(path.to_path_buf()), std::fs::canonicalize(path).ok()]
        .into_iter()
        .flatten()
    {
        if let Some(name) = candidate.file_name().and_then(|n| n.to_str()) {
            if let Some(version) = extract_version_from_filename(name) {
                return Some(version);
            }
        }
    }

    // Also check for versioned siblings in the same directory
    if let Some(parent) = path.parent() {
        if let Ok(entries) = std::fs::read_dir(parent) {
            for entry in entries.flatten() {
                if let Some(name) = entry.file_name().to_str() {
                    if name.starts_with("libonnxruntime") {
                        if let Some(version) = extract_version_from_filename(name) {
                            return Some(version);
                        }
                    }
                }
            }
        }
    }

    None
}

/// Extract version from filenames like "libonnxruntime.so.1.19.0" or "libonnxruntime.1.24.4.dylib"
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn extract_version_from_filename(name: &str) -> Option<String> {
    // Match patterns: .so.X.Y.Z or .X.Y.Z.dylib or .X.Y.Z.so
    let re = regex::Regex::new(r"(\d+\.\d+\.\d+)").ok()?;
    re.find(name).map(|m| m.as_str().to_string())
}

fn suggest_removal_command(lib_path: &str) -> String {
    if lib_path.starts_with("/usr/local/lib")
        || lib_path == "libonnxruntime.so"
        || lib_path == "libonnxruntime.dylib"
    {
        #[cfg(target_os = "linux")]
        return "   sudo rm /usr/local/lib/libonnxruntime* && sudo ldconfig".to_string();
        #[cfg(target_os = "macos")]
        return "   sudo rm /usr/local/lib/libonnxruntime*".to_string();
    }
    format!("   rm '{}'", lib_path)
}

/// Build the user-facing error message for an incompatible ONNX Runtime
/// install. Extracted as a pure helper so we can unit-test the wording
/// stability — the auto-fix recommendation must always come first because
/// it's the only safe option, and the system-rm step must remain present
/// because some users prefer the system-wide cleanup path.
pub(crate) fn format_ort_version_mismatch(version: &str, lib_name: &str) -> String {
    format!(
        "ONNX Runtime version mismatch: found v{} at '{}', but AFT requires v1.20+. \
         Solutions:\n\
         1. Auto-fix (recommended): run `npx @cortexkit/aft doctor --fix`. \
         This downloads AFT-managed ONNX Runtime v1.24 into AFT's storage and \
         configures the bridge to load it instead of the system library — no \
         changes to '{}'.\n\
         2. Remove the old library and restart (AFT auto-downloads the correct version on next start):\n\
         {}\n\
         3. Or install ONNX Runtime 1.24 system-wide: https://github.com/microsoft/onnxruntime/releases/tag/v1.24.0\n\
         4. Run `npx @cortexkit/aft doctor` for full diagnostics.",
        version,
        lib_name,
        lib_name,
        suggest_removal_command(lib_name),
    )
}

pub fn is_onnx_runtime_unavailable(message: &str) -> bool {
    if message
        .trim_start()
        .starts_with(ONNX_RUNTIME_MISSING_PREFIX)
    {
        return true;
    }

    let message = message.to_ascii_lowercase();
    let mentions_onnx_runtime = ["onnx runtime", "onnxruntime", "libonnxruntime"]
        .iter()
        .any(|pattern| message.contains(pattern));
    let mentions_dynamic_load_failure = [
        "shared library",
        "dynamic library",
        "failed to load",
        "could not load",
        "unable to load",
        "dlopen",
        "loadlibrary",
        "no such file",
        "not found",
    ]
    .iter()
    .any(|pattern| message.contains(pattern));

    mentions_onnx_runtime && mentions_dynamic_load_failure
}

pub fn format_embedding_init_error(error: impl Display) -> String {
    let message = error.to_string();

    if is_onnx_runtime_unavailable(&message) {
        return format!("{ONNX_RUNTIME_INSTALL_HINT} Original error: {message}");
    }

    format!("failed to initialize semantic embedding model: {message}")
}

/// A chunk of code ready for embedding — derived from a Symbol with context enrichment
#[derive(Debug, Clone)]
pub struct SemanticChunk {
    /// Absolute file path
    pub file: PathBuf,
    /// Symbol name
    pub name: String,
    /// Fully-qualified symbol name, when known from the outline scope chain.
    pub qualified_name: Option<String>,
    /// Symbol kind (function, class, struct, etc.)
    pub kind: SymbolKind,
    /// Line range (0-based internally, inclusive)
    pub start_line: u32,
    pub end_line: u32,
    /// Whether the symbol is exported
    pub exported: bool,
    /// The enriched text that gets embedded (name + file + kind + signature + body snippet)
    pub embed_text: String,
    /// Short code snippet for display in results
    pub snippet: String,
}

/// A stored embedding entry — chunk metadata + vector
#[derive(Debug, Clone)]
pub struct EmbeddingEntry {
    chunk: SemanticChunk,
    vector: Vec<f32>,
    /// Cached L2 norm so searches only recompute the query norm. Remote embedding
    /// backends do not guarantee unit vectors, so keep the actual norm instead of
    /// assuming it is 1.0.
    norm: f32,
}

impl EmbeddingEntry {
    fn new(chunk: SemanticChunk, vector: Vec<f32>) -> Self {
        let norm = vector_norm(&vector);
        Self {
            chunk,
            vector,
            norm,
        }
    }
}

enum BuildEmbeddingRow {
    Embedded {
        embedded_text: String,
        vector: Vec<f32>,
    },
    Skipped {
        embedded_text: String,
        reason: String,
    },
}

fn execute_build_embedding_batch<F>(
    texts: Vec<String>,
    embed_fn: &mut F,
) -> Result<Vec<BuildEmbeddingRow>, String>
where
    F: FnMut(Vec<String>) -> Result<Vec<Vec<f32>>, String>,
{
    clear_http_build_metadata();
    let requested_texts = texts.clone();
    let vectors = embed_fn(texts)?;
    let metadata = take_http_build_metadata();

    // Skipped rows carry an empty vector on purpose; the count check still
    // applies because every requested row must have exactly one slot.
    validate_embedding_batch_count(&vectors, requested_texts.len(), "embedding backend")?;
    if metadata
        .as_ref()
        .is_some_and(|metadata| metadata.len() != requested_texts.len())
    {
        return Err("embedding backend returned mismatched row metadata".to_string());
    }

    let metadata = metadata.unwrap_or_else(|| {
        requested_texts
            .into_iter()
            .map(|embedded_text| BuildEmbeddingRowMetadata {
                embedded_text,
                skipped_reason: None,
            })
            .collect()
    });
    let mut expected_dimension = None;
    let mut rows = Vec::with_capacity(vectors.len());
    for (metadata, vector) in metadata.into_iter().zip(vectors) {
        if let Some(reason) = metadata.skipped_reason {
            if !vector.is_empty() {
                return Err("skipped embedding row unexpectedly returned a vector".to_string());
            }
            rows.push(BuildEmbeddingRow::Skipped {
                embedded_text: metadata.embedded_text,
                reason,
            });
            continue;
        }

        validate_embedding_dimension(vector.len())
            .map_err(|error| format!("embedding backend returned {error}"))?;
        match expected_dimension {
            None => expected_dimension = Some(vector.len()),
            Some(expected) if expected != vector.len() => {
                return Err(format!(
                    "embedding backend returned inconsistent embedding dimensions: expected {expected}, got {}",
                    vector.len()
                ));
            }
            _ => {}
        }
        rows.push(BuildEmbeddingRow::Embedded {
            embedded_text: metadata.embedded_text,
            vector,
        });
    }

    Ok(rows)
}

fn format_skipped_row_warning(chunk: &SemanticChunk, embedded_text: &str, reason: &str) -> String {
    format!(
        "semantic embed skipped row: file={} symbol={} chars={} reason={}",
        chunk.file.display(),
        chunk.name,
        embedded_text.chars().count(),
        reason,
    )
}

fn log_skipped_row_warning(chunk: &SemanticChunk, embedded_text: &str, reason: &str) {
    let warning = format_skipped_row_warning(chunk, embedded_text, reason);
    #[cfg(test)]
    TEST_SKIPPED_ROW_WARNINGS.with(|warnings| warnings.borrow_mut().push(warning.clone()));
    slog_warn!("{}", warning);
}

#[cfg(test)]
fn take_test_skipped_row_warnings() -> Vec<String> {
    TEST_SKIPPED_ROW_WARNINGS.with(|warnings| std::mem::take(&mut *warnings.borrow_mut()))
}

#[derive(Debug)]
struct SharedSemanticBase {
    entries: Vec<EmbeddingEntry>,
    file_mtimes: HashMap<PathBuf, SystemTime>,
    file_sizes: HashMap<PathBuf, u64>,
    any_missing_sizes: bool,
    file_hashes: HashMap<PathBuf, blake3::Hash>,
    dimension: usize,
    fingerprint: Option<SemanticIndexFingerprint>,
    deferred_files: HashSet<PathBuf>,
    skipped_rows: usize,
    dirty_paths: Arc<Mutex<Option<BTreeSet<PathBuf>>>>,
    persistence: Arc<Mutex<Option<SemanticPersistenceState>>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SharedSemanticBaseKey {
    artifact_cache_key: String,
    fingerprint: String,
    artifact_content_hash: blake3::Hash,
}

type SharedSemanticBaseRegistry = HashMap<SharedSemanticBaseKey, Weak<SharedSemanticBase>>;

fn shared_semantic_bases() -> &'static Mutex<SharedSemanticBaseRegistry> {
    static REGISTRY: OnceLock<Mutex<SharedSemanticBaseRegistry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

static SHARED_SEMANTIC_BASE_LOADS: AtomicUsize = AtomicUsize::new(0);
static SHARED_SEMANTIC_BASE_HITS: AtomicUsize = AtomicUsize::new(0);

impl SharedSemanticBase {
    fn estimated_memory(&self) -> crate::memory::MemoryEstimate {
        let vector_bytes = self.entries.iter().fold(0u64, |bytes, entry| {
            bytes.saturating_add(
                crate::memory::usize_to_u64(entry.vector.len())
                    .saturating_mul(std::mem::size_of::<f32>() as u64),
            )
        });
        let text_bytes = self.entries.iter().fold(0u64, |bytes, entry| {
            bytes
                .saturating_add(crate::memory::path_bytes(&entry.chunk.file))
                .saturating_add(crate::memory::usize_to_u64(entry.chunk.name.len()))
                .saturating_add(
                    entry
                        .chunk
                        .qualified_name
                        .as_ref()
                        .map(|name| crate::memory::usize_to_u64(name.len()))
                        .unwrap_or(0),
                )
                .saturating_add(crate::memory::usize_to_u64(entry.chunk.embed_text.len()))
                .saturating_add(crate::memory::usize_to_u64(entry.chunk.snippet.len()))
        });
        let metadata_bytes = crate::memory::usize_to_u64(self.entries.len())
            .saturating_mul(std::mem::size_of::<EmbeddingEntry>() as u64)
            .saturating_add(
                self.file_mtimes
                    .keys()
                    .chain(self.file_sizes.keys())
                    .chain(self.file_hashes.keys())
                    .chain(self.deferred_files.iter())
                    .map(|path| crate::memory::path_bytes(path))
                    .fold(0u64, u64::saturating_add),
            )
            .saturating_add(
                crate::memory::usize_to_u64(self.file_mtimes.len())
                    .saturating_mul(std::mem::size_of::<SystemTime>() as u64),
            )
            .saturating_add(
                crate::memory::usize_to_u64(self.file_sizes.len())
                    .saturating_mul(std::mem::size_of::<u64>() as u64),
            )
            .saturating_add(
                crate::memory::usize_to_u64(self.file_hashes.len())
                    .saturating_mul(std::mem::size_of::<blake3::Hash>() as u64),
            );
        crate::memory::MemoryEstimate::estimated(
            vector_bytes
                .saturating_add(text_bytes)
                .saturating_add(metadata_bytes),
        )
        .count("entries", self.entries.len())
        .count("indexed_files", self.file_mtimes.len())
        .count_u64("vector_bytes", vector_bytes)
        .count_u64("text_bytes", text_bytes)
        .count_u64("metadata_bytes", metadata_bytes)
    }
}

pub(crate) fn shared_semantic_bases_memory() -> crate::memory::MemoryEstimate {
    let mut registry = shared_semantic_bases()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    registry.retain(|_, base| base.strong_count() > 0);
    let bases = registry
        .values()
        .filter_map(Weak::upgrade)
        .collect::<Vec<_>>();
    let estimates = bases
        .iter()
        .map(|base| base.estimated_memory())
        .collect::<Vec<_>>();
    let bytes = estimates.iter().fold(0u64, |sum, estimate| {
        sum.saturating_add(estimate.estimated_bytes.unwrap_or(0))
    });
    let count_bytes = |name: &str| {
        estimates.iter().fold(0u64, |sum, estimate| {
            sum.saturating_add(estimate.counts.get(name).copied().unwrap_or(0))
        })
    };
    crate::memory::MemoryEstimate::estimated(bytes)
        .count("bases", bases.len())
        .count("entries", bases.iter().map(|base| base.entries.len()).sum())
        .count_u64("vector_bytes", count_bytes("vector_bytes"))
        .count_u64("text_bytes", count_bytes("text_bytes"))
        .count_u64("metadata_bytes", count_bytes("metadata_bytes"))
        .count_u64(
            "loads",
            SHARED_SEMANTIC_BASE_LOADS.load(Ordering::Relaxed) as u64,
        )
        .count_u64(
            "hits",
            SHARED_SEMANTIC_BASE_HITS.load(Ordering::Relaxed) as u64,
        )
}

fn borrowed_artifact_identity(data_path: &Path) -> Result<(String, blake3::Hash), String> {
    let mut file = fs::File::open(data_path).map_err(|error| error.to_string())?;
    let mut hasher = blake3::Hasher::new();
    hasher
        .update_reader(&mut file)
        .map_err(|error| error.to_string())?;
    let artifact_content_hash = hasher.finalize();

    let mut header = BufReader::new(fs::File::open(data_path).map_err(|error| error.to_string())?);
    let mut fixed = [0u8; HEADER_BYTES_V2];
    header
        .read_exact(&mut fixed)
        .map_err(|error| error.to_string())?;
    if fixed[0] != SEMANTIC_INDEX_VERSION_V6 && fixed[0] != SEMANTIC_INDEX_VERSION_V7 {
        return Err(format!(
            "unsupported semantic artifact version {}",
            fixed[0]
        ));
    }
    let fingerprint_len = u32::from_le_bytes(fixed[9..13].try_into().unwrap()) as usize;
    if fingerprint_len == 0 || fingerprint_len > 64 * 1024 {
        return Err("semantic artifact fingerprint is missing or oversized".to_string());
    }
    let mut fingerprint = vec![0u8; fingerprint_len];
    header
        .read_exact(&mut fingerprint)
        .map_err(|error| error.to_string())?;
    let fingerprint = String::from_utf8(fingerprint).map_err(|error| error.to_string())?;
    Ok((fingerprint, artifact_content_hash))
}

/// The semantic index — stores embeddings for all symbols in a project.
/// Borrow-only roots retain only a root path plus an Arc to immutable relative data.
#[derive(Debug, Clone)]
pub struct SemanticIndex {
    entries: Vec<EmbeddingEntry>,
    /// Track which files are indexed and their mtime for staleness detection
    file_mtimes: HashMap<PathBuf, SystemTime>,
    /// Track indexed file sizes alongside mtimes for staleness detection
    file_sizes: HashMap<PathBuf, u64>,
    /// Avoid walking every indexed path on warm refreshes once size metadata is complete.
    any_missing_sizes: bool,
    file_hashes: HashMap<PathBuf, blake3::Hash>,
    /// Embedding dimension (384 for MiniLM-L6-v2)
    dimension: usize,
    fingerprint: Option<SemanticIndexFingerprint>,
    project_root: PathBuf,
    deferred_files: HashSet<PathBuf>,
    shared_base: Option<Arc<SharedSemanticBase>>,
    /// Paths whose complete persisted rows must replace prior rows. `None` is
    /// reserved for indexes created by callers that cannot report mutations.
    dirty_paths: Arc<Mutex<Option<BTreeSet<PathBuf>>>>,
    persistence: Arc<Mutex<Option<SemanticPersistenceState>>>,
    last_append_read_bytes: Arc<AtomicUsize>,
    /// Rows rejected by the backend even after bounded body shrinking.
    skipped_rows: usize,
    #[cfg(test)]
    removal_retain_passes: usize,
}

#[derive(Debug, Clone, Copy)]
struct IndexedFileMetadata {
    mtime: SystemTime,
    size: u64,
    content_hash: blake3::Hash,
}

#[derive(Debug, Default, Clone, Copy)]
struct SemanticCollectPhaseTimings {
    sched: Duration,
    read_hash: Duration,
    parse: Duration,
    extract: Duration,
    build: Duration,
}

impl SemanticCollectPhaseTimings {
    fn add_assign(&mut self, other: Self) {
        self.sched += other.sched;
        self.read_hash += other.read_hash;
        self.parse += other.parse;
        self.extract += other.extract;
        self.build += other.build;
    }
}

type CollectedSemanticFile = (
    PathBuf,
    Result<(IndexedFileMetadata, Vec<SemanticChunk>), String>,
    SemanticCollectPhaseTimings,
);

/// Result of an incremental refresh of the semantic index. Counts are file
/// counts; `total_processed` is the number of current/deleted files considered.
#[derive(Debug, Default, Clone, Copy)]
pub struct RefreshSummary {
    pub changed: usize,
    pub added: usize,
    pub deleted: usize,
    pub total_processed: usize,
}

impl RefreshSummary {
    /// True when no files were touched.
    pub fn is_noop(&self) -> bool {
        self.changed == 0 && self.added == 0 && self.deleted == 0
    }
}

#[derive(Debug, Default)]
pub struct InvalidatedFilesRefresh {
    /// Full replacement entries for `completed_paths`, not just newly embedded
    /// chunks. `apply_refresh_update` removes completed paths before extending
    /// this set, so reused chunks must travel in this delta too.
    pub added_entries: Vec<EmbeddingEntry>,
    pub updated_metadata: Vec<(PathBuf, FileFreshness)>,
    pub completed_paths: Vec<PathBuf>,
    pub summary: RefreshSummary,
}

#[derive(Debug, Clone)]
struct ReusableEmbedding {
    embed_text: String,
    vector: Vec<f32>,
}

type ChunkReuseMap = HashMap<PathBuf, HashMap<blake3::Hash, Vec<ReusableEmbedding>>>;

const SEMANTIC_BLOB_PAYLOAD_VERSION: u8 = 1;

fn extend_reuse_map_from_semantic_blob(
    reuse_map: &mut ChunkReuseMap,
    file: &Path,
    payload: &[u8],
    expected_fingerprint: &str,
    expected_dimension: usize,
) -> Result<(), String> {
    let mut reader = CountingReader::with_bytes_read(Cursor::new(payload), 0);
    let version = read_u8_stream(&mut reader, "missing semantic blob version")?;
    if version != SEMANTIC_BLOB_PAYLOAD_VERSION {
        return Err(format!("unsupported semantic blob version {version}"));
    }
    for (label, expected) in [
        ("chunker", crate::blob_store::SEMANTIC_PRODUCER_VERSION),
        ("template", crate::blob_store::SEMANTIC_PRODUCER_VERSION),
        ("model", expected_fingerprint),
    ] {
        let actual = read_string_stream(&mut reader, Some(payload.len()))?;
        if actual != expected {
            return Err(format!("semantic blob {label} fingerprint mismatch"));
        }
    }
    let entry_count = read_u32_stream(&mut reader)? as usize;
    if entry_count > MAX_ENTRIES {
        return Err(format!("too many semantic blob entries {entry_count}"));
    }
    let vector_bytes = expected_dimension
        .checked_mul(F32_BYTES)
        .ok_or_else(|| "semantic blob vector length overflow".to_string())?;
    for _ in 0..entry_count {
        let _name = read_string_stream(&mut reader, Some(payload.len()))?;
        let _qualified_name = read_string_stream(&mut reader, Some(payload.len()))?;
        let _kind = read_u8_stream(&mut reader, "missing semantic blob symbol kind")?;
        let _start_line = read_u32_stream(&mut reader)?;
        let _end_line = read_u32_stream(&mut reader)?;
        let _exported = read_u8_stream(&mut reader, "missing semantic blob export flag")?;
        let _snippet = read_string_stream(&mut reader, Some(payload.len()))?;
        let embed_text = read_string_stream(&mut reader, Some(payload.len()))?;
        let raw_vector = read_blob_bytes(&mut reader, payload.len())?;
        if raw_vector.len() != vector_bytes {
            return Err(format!(
                "semantic blob vector has {} bytes, expected {vector_bytes}",
                raw_vector.len()
            ));
        }
        let vector = raw_vector
            .chunks_exact(F32_BYTES)
            .map(|bytes| f32::from_le_bytes(bytes.try_into().expect("four-byte float")))
            .collect::<Vec<_>>();
        reuse_map
            .entry(file.to_path_buf())
            .or_default()
            .entry(blake3::hash(embed_text.as_bytes()))
            .or_default()
            .push(ReusableEmbedding { embed_text, vector });
    }
    if reader.bytes_read() != payload.len() {
        return Err("trailing bytes after semantic blob payload".to_string());
    }
    Ok(())
}

fn read_blob_bytes<R: Read>(
    reader: &mut CountingReader<R>,
    total_len: usize,
) -> Result<Vec<u8>, String> {
    let len = read_u32_stream(reader)? as usize;
    if reader.bytes_read().saturating_add(len) > total_len {
        return Err("unexpected end of semantic blob bytes".to_string());
    }
    let mut bytes = vec![0; len];
    read_exact_stream(reader, &mut bytes, "unexpected end of semantic blob bytes")?;
    Ok(bytes)
}

/// Search result from a semantic query
#[derive(Debug, Clone)]
pub struct SemanticResult {
    pub file: PathBuf,
    pub name: String,
    pub qualified_name: Option<String>,
    pub kind: SymbolKind,
    pub start_line: u32,
    pub end_line: u32,
    pub exported: bool,
    pub snippet: String,
    pub score: f32,
    pub rank_score: f32,
    pub cap_protected: bool,
    pub source: &'static str,
}

fn relativize_semantic_map<T>(
    project_root: &Path,
    map: HashMap<PathBuf, T>,
) -> Option<HashMap<PathBuf, T>> {
    map.into_iter()
        .map(|(path, value)| cache_relative_path(project_root, &path).map(|path| (path, value)))
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SemanticArtifactIdentity {
    bytes: u64,
    modified_nanos: Option<u128>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SemanticPersistenceState {
    identity: SemanticArtifactIdentity,
    base_bytes: usize,
    segment_count: usize,
    segment_bytes: usize,
    valid_bytes: usize,
}

#[derive(Debug, Clone, Copy)]
struct SemanticArtifactLayout {
    identity: SemanticArtifactIdentity,
    base_bytes: usize,
    valid_bytes: usize,
    segment_count: usize,
    segment_bytes: usize,
    torn_tail: bool,
    bytes_read: usize,
}

#[derive(Debug)]
struct LoadedSemanticArtifact {
    index: SemanticIndex,
    base_bytes: usize,
    valid_bytes: usize,
    segment_count: usize,
    segment_bytes: usize,
    torn_tail: bool,
}

fn semantic_artifact_identity(path: &Path) -> Option<SemanticArtifactIdentity> {
    let metadata = path.metadata().ok()?;
    let modified_nanos = metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos());
    Some(SemanticArtifactIdentity {
        bytes: metadata.len(),
        modified_nanos,
    })
}

fn semantic_persistence_lock_wait(artifact_bytes: u64) -> Duration {
    let proportional_seconds = artifact_bytes
        .div_ceil(SEMANTIC_PERSIST_LOCK_BYTES_PER_SECOND)
        .saturating_add(2);
    SEMANTIC_PERSIST_LOCK_MIN_WAIT.max(Duration::from_secs(proportional_seconds))
}

fn acquire_semantic_persistence_lock(
    dir: &Path,
    artifact_bytes: u64,
) -> io::Result<fs_lock::LockGuard> {
    fs_lock::try_acquire(
        &dir.join("semantic.persist.lock"),
        semantic_persistence_lock_wait(artifact_bytes),
    )
    .map_err(|error| match error {
        fs_lock::AcquireError::Timeout => {
            io::Error::other("timed out acquiring semantic persistence lock")
        }
        fs_lock::AcquireError::Io(error) => error,
    })
}

fn semantic_entry_cmp(left: &&EmbeddingEntry, right: &&EmbeddingEntry) -> std::cmp::Ordering {
    let left = *left;
    let right = *right;
    left.chunk
        .file
        .cmp(&right.chunk.file)
        .then_with(|| left.chunk.name.cmp(&right.chunk.name))
        .then_with(|| left.chunk.qualified_name.cmp(&right.chunk.qualified_name))
        .then_with(|| {
            symbol_kind_to_u8(&left.chunk.kind).cmp(&symbol_kind_to_u8(&right.chunk.kind))
        })
        .then_with(|| left.chunk.start_line.cmp(&right.chunk.start_line))
        .then_with(|| left.chunk.end_line.cmp(&right.chunk.end_line))
        .then_with(|| left.chunk.exported.cmp(&right.chunk.exported))
        .then_with(|| left.chunk.snippet.cmp(&right.chunk.snippet))
        .then_with(|| left.chunk.embed_text.cmp(&right.chunk.embed_text))
        .then_with(|| {
            left.vector
                .iter()
                .map(|value| value.to_bits())
                .cmp(right.vector.iter().map(|value| value.to_bits()))
        })
}

fn semantic_entry_persistence_eq(left: &EmbeddingEntry, right: &EmbeddingEntry) -> bool {
    left.chunk.file == right.chunk.file
        && left.chunk.name == right.chunk.name
        && left.chunk.qualified_name == right.chunk.qualified_name
        && left.chunk.kind == right.chunk.kind
        && left.chunk.start_line == right.chunk.start_line
        && left.chunk.end_line == right.chunk.end_line
        && left.chunk.exported == right.chunk.exported
        && left.chunk.snippet == right.chunk.snippet
        && left.chunk.embed_text == right.chunk.embed_text
        && left.vector.len() == right.vector.len()
        && left
            .vector
            .iter()
            .zip(&right.vector)
            .all(|(left, right)| left.to_bits() == right.to_bits())
}

fn semantic_entries_by_file(index: &SemanticIndex) -> HashMap<&Path, Vec<&EmbeddingEntry>> {
    let mut by_file: HashMap<&Path, Vec<&EmbeddingEntry>> = HashMap::new();
    for entry in &index.entries {
        by_file
            .entry(entry.chunk.file.as_path())
            .or_default()
            .push(entry);
    }
    for entries in by_file.values_mut() {
        entries.sort_by(semantic_entry_cmp);
    }
    by_file
}

fn semantic_changed_paths(previous: &SemanticIndex, current: &SemanticIndex) -> BTreeSet<PathBuf> {
    let previous_entries = semantic_entries_by_file(previous);
    let current_entries = semantic_entries_by_file(current);
    let mut paths = BTreeSet::new();
    paths.extend(previous.file_mtimes.keys().cloned());
    paths.extend(current.file_mtimes.keys().cloned());
    paths.extend(previous_entries.keys().map(|path| (*path).to_path_buf()));
    paths.extend(current_entries.keys().map(|path| (*path).to_path_buf()));
    paths
        .into_iter()
        .filter(|path| {
            if previous.file_mtimes.get(path) != current.file_mtimes.get(path)
                || previous.file_sizes.get(path) != current.file_sizes.get(path)
                || previous.file_hashes.get(path) != current.file_hashes.get(path)
            {
                return true;
            }
            let previous = previous_entries
                .get(path.as_path())
                .map(Vec::as_slice)
                .unwrap_or_default();
            let current = current_entries
                .get(path.as_path())
                .map(Vec::as_slice)
                .unwrap_or_default();
            previous.len() != current.len()
                || !previous
                    .iter()
                    .zip(current)
                    .all(|(previous, current)| semantic_entry_persistence_eq(previous, current))
        })
        .collect()
}

impl SemanticIndex {
    fn from_shared_base(project_root: PathBuf, shared_base: Arc<SharedSemanticBase>) -> Self {
        debug_assert!(project_root.is_absolute());
        Self {
            entries: Vec::new(),
            file_mtimes: HashMap::new(),
            file_sizes: HashMap::new(),
            any_missing_sizes: false,
            file_hashes: HashMap::new(),
            dimension: shared_base.dimension,
            fingerprint: shared_base.fingerprint.clone(),
            project_root,
            deferred_files: HashSet::new(),
            dirty_paths: Arc::clone(&shared_base.dirty_paths),
            persistence: Arc::clone(&shared_base.persistence),
            last_append_read_bytes: Arc::new(AtomicUsize::new(0)),
            skipped_rows: shared_base.skipped_rows,
            shared_base: Some(shared_base),
            #[cfg(test)]
            removal_retain_passes: 0,
        }
    }

    pub(crate) fn adopt_frozen_base_for_root(
        &mut self,
        project_root: &Path,
        config: &SemanticBackendConfig,
    ) -> Option<Self> {
        let expected = SemanticIndexFingerprint::for_config_dimension(config, self.dimension());
        if !self
            .fingerprint()
            .is_some_and(|fingerprint| fingerprint.matches(&expected))
        {
            return None;
        }

        if let Some(base) = self.shared_base.as_ref() {
            return Some(Self::from_shared_base(
                project_root.to_path_buf(),
                Arc::clone(base),
            ));
        }

        if !self.paths_are_shareable() {
            return None;
        }

        // Move the resident vectors into one immutable relative-path base rather
        // than cloning them. The owner and each matching worktree then retain
        // only an Arc plus their own root for path projection.
        let owner_root = self.project_root.clone();
        let placeholder = Self::new(owner_root.clone(), self.dimension());
        let private = std::mem::replace(self, placeholder);
        let base = match private.into_shared_base() {
            Ok(base) => Arc::new(base),
            Err(private) => {
                // Unreachable after the shareability check (this index is held
                // exclusively, so no path can appear between the check and the
                // move), but a private index is never worth a process: restore
                // it and decline to share.
                crate::slog_warn!(
                    "semantic index for {} could not be frozen into a shared base; keeping it private",
                    owner_root.display()
                );
                *self = private;
                return None;
            }
        };
        *self = Self::from_shared_base(owner_root, Arc::clone(&base));
        Some(Self::from_shared_base(project_root.to_path_buf(), base))
    }

    /// Every path this index carries must be expressible relative to its own
    /// root before the index can be frozen into a base shared across roots.
    /// The dirty-path set belongs here too: it is persisted with the base, and
    /// a delta path outside the root once turned the freeze into a panic.
    fn paths_are_shareable(&self) -> bool {
        let shareable = |path: &Path| cache_relative_path(&self.project_root, path).is_some();
        self.entries
            .iter()
            .all(|entry| shareable(&entry.chunk.file))
            && self
                .file_mtimes
                .keys()
                .chain(self.file_sizes.keys())
                .chain(self.file_hashes.keys())
                .chain(self.deferred_files.iter())
                .all(|path| shareable(path))
            && self
                .dirty_paths
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
                .is_none_or(|paths| paths.iter().all(|path| shareable(path)))
    }

    fn into_shared_base(mut self) -> Result<SharedSemanticBase, Self> {
        // Relativize every path before moving anything, so a path outside the
        // root hands the index back intact instead of leaving a half-moved
        // one behind. Only the path strings are copied here; the vectors move.
        let root = self.project_root.clone();
        let relative = |path: &Path| cache_relative_path(&root, path);
        let Some(entry_files) = self
            .entries
            .iter()
            .map(|entry| relative(&entry.chunk.file))
            .collect::<Option<Vec<_>>>()
        else {
            return Err(self);
        };
        let Some(deferred_files) = self
            .deferred_files
            .iter()
            .map(|path| relative(path))
            .collect::<Option<HashSet<_>>>()
        else {
            return Err(self);
        };
        let dirty_paths = {
            let guard = self
                .dirty_paths
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match guard.as_ref() {
                Some(paths) => paths
                    .iter()
                    .map(|path| relative(path))
                    .collect::<Option<BTreeSet<_>>>()
                    .map(Some),
                None => Some(None),
            }
        };
        let Some(dirty_paths) = dirty_paths else {
            return Err(self);
        };
        let (Some(file_mtimes), Some(file_sizes), Some(file_hashes)) = (
            relativize_semantic_map(&root, self.file_mtimes.clone()),
            relativize_semantic_map(&root, self.file_sizes.clone()),
            relativize_semantic_map(&root, self.file_hashes.clone()),
        ) else {
            return Err(self);
        };
        for (entry, file) in self.entries.iter_mut().zip(entry_files) {
            entry.chunk.file = file;
        }
        let persistence = *self
            .persistence
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(SharedSemanticBase {
            entries: self.entries,
            file_mtimes,
            file_sizes,
            any_missing_sizes: self.any_missing_sizes,
            file_hashes,
            dimension: self.dimension,
            fingerprint: self.fingerprint,
            deferred_files,
            skipped_rows: self.skipped_rows,
            dirty_paths: Arc::new(Mutex::new(dirty_paths)),
            persistence: Arc::new(Mutex::new(persistence)),
        })
    }

    fn materialize_shared_base(&mut self) {
        let Some(base) = self.shared_base.take() else {
            return;
        };
        self.entries = base
            .entries
            .iter()
            .cloned()
            .map(|mut entry| {
                entry.chunk.file = self.project_root.join(&entry.chunk.file);
                entry
            })
            .collect();
        self.file_mtimes = base
            .file_mtimes
            .iter()
            .map(|(path, value)| (self.project_root.join(path), *value))
            .collect();
        self.file_sizes = base
            .file_sizes
            .iter()
            .map(|(path, value)| (self.project_root.join(path), *value))
            .collect();
        self.any_missing_sizes = base.any_missing_sizes;
        self.file_hashes = base
            .file_hashes
            .iter()
            .map(|(path, value)| (self.project_root.join(path), *value))
            .collect();
        self.dimension = base.dimension;
        self.fingerprint = base.fingerprint.clone();
        self.skipped_rows = base.skipped_rows;
        self.deferred_files = base
            .deferred_files
            .iter()
            .map(|path| self.project_root.join(path))
            .collect();
        let dirty_paths = base
            .dirty_paths
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map(|paths| {
                paths
                    .iter()
                    .map(|path| self.project_root.join(path))
                    .collect()
            });
        let persistence = *base
            .persistence
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.set_dirty_paths(dirty_paths);
        self.set_persistence(persistence);
    }

    pub fn new(project_root: PathBuf, dimension: usize) -> Self {
        debug_assert!(project_root.is_absolute());
        Self {
            entries: Vec::new(),
            file_mtimes: HashMap::new(),
            file_sizes: HashMap::new(),
            any_missing_sizes: false,
            file_hashes: HashMap::new(),
            dimension,
            fingerprint: None,
            project_root,
            deferred_files: HashSet::new(),
            shared_base: None,
            dirty_paths: Arc::new(Mutex::new(None)),
            persistence: Arc::new(Mutex::new(None)),
            last_append_read_bytes: Arc::new(AtomicUsize::new(0)),
            skipped_rows: 0,
            #[cfg(test)]
            removal_retain_passes: 0,
        }
    }

    /// Number of rows omitted because the backend still rejected their header floor.
    pub fn skipped_rows(&self) -> usize {
        self.skipped_rows
    }

    /// Number of embedded symbol entries.
    pub fn entry_count(&self) -> usize {
        self.shared_base
            .as_ref()
            .map(|base| base.entries.len())
            .unwrap_or_else(|| self.entries.len())
    }

    /// Estimate resident semantic-index bytes from the vectors and metadata
    /// actually held by each entry. This intentionally excludes allocator and
    /// hash-table bucket overhead, which are not cheaply observable.
    pub fn estimated_memory(&self) -> crate::memory::MemoryEstimate {
        if let Some(base) = &self.shared_base {
            return crate::memory::MemoryEstimate::estimated(0)
                .count("entries", base.entries.len())
                .count("dimensions", base.dimension)
                .count("indexed_files", base.file_mtimes.len())
                .count("shared_base_entries", base.entries.len())
                .count("overlay_entries", 0)
                .count_u64("vector_bytes", 0)
                .count_u64("text_bytes", 0)
                .count_u64("metadata_bytes", 0);
        }
        if self.entries.is_empty()
            && self.file_mtimes.is_empty()
            && self.file_sizes.is_empty()
            && self.file_hashes.is_empty()
            && self.deferred_files.is_empty()
        {
            return crate::memory::MemoryEstimate::estimated(0)
                .count("entries", 0)
                .count("dimensions", self.dimension)
                .count("indexed_files", 0)
                .count_u64("vector_bytes", 0)
                .count_u64("text_bytes", 0)
                .count_u64("metadata_bytes", 0)
                .count_u64("average_text_bytes", 0)
                .count_u64("average_metadata_bytes", 0);
        }
        let vector_bytes = self.entries.iter().fold(0u64, |bytes, entry| {
            bytes.saturating_add(
                crate::memory::usize_to_u64(entry.vector.len())
                    .saturating_mul(std::mem::size_of::<f32>() as u64),
            )
        });
        let text_bytes = self.entries.iter().fold(0u64, |bytes, entry| {
            let chunk = &entry.chunk;
            bytes
                .saturating_add(crate::memory::path_bytes(&chunk.file))
                .saturating_add(crate::memory::usize_to_u64(chunk.name.len()))
                .saturating_add(
                    chunk
                        .qualified_name
                        .as_ref()
                        .map(|name| crate::memory::usize_to_u64(name.len()))
                        .unwrap_or(0),
                )
                .saturating_add(crate::memory::usize_to_u64(chunk.embed_text.len()))
                .saturating_add(crate::memory::usize_to_u64(chunk.snippet.len()))
        });
        let entry_metadata_bytes = crate::memory::usize_to_u64(self.entries.len())
            .saturating_mul(std::mem::size_of::<EmbeddingEntry>() as u64);
        let file_metadata_bytes = self
            .file_mtimes
            .keys()
            .chain(self.file_sizes.keys())
            .chain(self.file_hashes.keys())
            .chain(self.deferred_files.iter())
            .map(|path| crate::memory::path_bytes(path))
            .fold(0u64, u64::saturating_add)
            .saturating_add(
                crate::memory::usize_to_u64(self.file_mtimes.len())
                    .saturating_mul(std::mem::size_of::<SystemTime>() as u64),
            )
            .saturating_add(
                crate::memory::usize_to_u64(self.file_sizes.len())
                    .saturating_mul(std::mem::size_of::<u64>() as u64),
            )
            .saturating_add(
                crate::memory::usize_to_u64(self.file_hashes.len())
                    .saturating_mul(std::mem::size_of::<blake3::Hash>() as u64),
            );
        let index_metadata_bytes = crate::memory::path_bytes(&self.project_root).saturating_add(
            self.fingerprint
                .as_ref()
                .map(|fingerprint| {
                    crate::memory::usize_to_u64(fingerprint.backend.len())
                        .saturating_add(crate::memory::usize_to_u64(fingerprint.model.len()))
                        .saturating_add(crate::memory::usize_to_u64(fingerprint.base_url.len()))
                })
                .unwrap_or(0),
        );
        let metadata_bytes = entry_metadata_bytes
            .saturating_add(file_metadata_bytes)
            .saturating_add(index_metadata_bytes);
        let entry_count = crate::memory::usize_to_u64(self.entries.len());
        crate::memory::MemoryEstimate::estimated(
            vector_bytes
                .saturating_add(text_bytes)
                .saturating_add(metadata_bytes),
        )
        .count("entries", self.entries.len())
        .count("dimensions", self.dimension)
        .count("indexed_files", self.file_mtimes.len())
        .count_u64("vector_bytes", vector_bytes)
        .count_u64("text_bytes", text_bytes)
        .count_u64("metadata_bytes", metadata_bytes)
        .count_u64(
            "average_text_bytes",
            text_bytes.checked_div(entry_count).unwrap_or(0),
        )
        .count_u64(
            "average_metadata_bytes",
            metadata_bytes.checked_div(entry_count).unwrap_or(0),
        )
    }

    /// Number of files currently tracked by the semantic index.
    pub fn indexed_file_count(&self) -> usize {
        self.shared_base
            .as_ref()
            .map(|base| base.file_mtimes.len())
            .unwrap_or_else(|| self.file_mtimes.len())
    }

    /// Status word for an index object the daemon has already loaded.
    ///
    /// The daemon-held status decides the word; the entry count only chooses
    /// between the two healthy ones. Deciding from the entry count alone could
    /// not say "failed" at all, so `aft status` answered "is semantic search
    /// working?" with `ready` for a root whose index had died — while the
    /// sidebar and a search reply both reported the failure correctly. The
    /// surface a user checks first was the one surface that could not say no.
    ///
    /// A build in progress reports `loading` even when a previous index object
    /// is still installed: that object is not what the daemon is serving from,
    /// and reporting it as `ready` is the same substitution in a milder form.
    pub fn status_label(&self, status: &SemanticIndexStatus) -> &'static str {
        match status {
            SemanticIndexStatus::Failed(_) => "failed",
            SemanticIndexStatus::Disabled => "disabled",
            SemanticIndexStatus::Building { .. } => "loading",
            SemanticIndexStatus::Ready { .. } if self.entry_count() == 0 => "empty",
            SemanticIndexStatus::Ready { .. } => "ready",
        }
    }

    fn collect_chunks(
        project_root: &Path,
        files: &[PathBuf],
        embed_text_caps: EmbedTextCaps,
    ) -> (Vec<SemanticChunk>, HashMap<PathBuf, IndexedFileMetadata>) {
        let collect_started = Instant::now();
        let collect_one = |file: &Path, sched: Duration| {
            let mut phases = SemanticCollectPhaseTimings {
                sched,
                ..SemanticCollectPhaseTimings::default()
            };
            let result = collect_semantic_file(project_root, file, embed_text_caps, &mut phases);
            (file.to_path_buf(), result, phases)
        };
        let per_file: Vec<CollectedSemanticFile> = if files.len() <= 2 {
            files
                .iter()
                .map(|file| collect_one(file, Duration::ZERO))
                .collect()
        } else {
            files
                .par_iter()
                .map(|file| collect_one(file, collect_started.elapsed()))
                .collect()
        };

        let mut chunks: Vec<SemanticChunk> = Vec::new();
        let mut file_metadata: HashMap<PathBuf, IndexedFileMetadata> = HashMap::new();
        let mut phases = SemanticCollectPhaseTimings::default();

        for (file, result, file_phases) in per_file {
            phases.add_assign(file_phases);
            match result {
                Ok((metadata, file_chunks)) => {
                    file_metadata.insert(file, metadata);
                    chunks.extend(file_chunks);
                }
                Err(error) => {
                    // "unsupported file extension" is expected for non-code files
                    // (json, xml, .gitignore, etc.) that get included in the
                    // project walk. Pre-fix this was swallowed by .unwrap_or_default();
                    // we now skip silently to keep the log clean. Only real read/parse
                    // errors are worth surfacing.
                    if error == "unsupported file extension" {
                        continue;
                    }
                    slog_warn!(
                        "failed to collect semantic chunks for {}: {}",
                        file.display(),
                        error
                    );
                }
            }
        }

        let collect_ms = collect_started
            .elapsed()
            .as_millis()
            .min(u128::from(u64::MAX)) as u64;
        crate::logging::note_semantic_collect(chunks.len(), file_metadata.len(), collect_ms);
        slog_info!(
            "semantic collect: {} chunks from {} files in {} ms",
            chunks.len(),
            file_metadata.len(),
            collect_ms
        );
        if let Some(scope) = crate::logging::current_index_build() {
            if scope.plane == crate::logging::IndexPlane::Semantic {
                crate::logging::log_index_event(
                    crate::logging::IndexEvent::from_scope(
                        crate::logging::IndexEventKind::BuildProgress,
                        &scope,
                    )
                    .field("stage", "collect")
                    .field("completed", 1)
                    .field("total", 1)
                    .field("elapsed_ms", scope.elapsed_ms())
                    .field("chunks", chunks.len())
                    .field("files", file_metadata.len()),
                );
            }
        }
        if collect_ms > 50 {
            slog_info!(
                "semantic collect phases: sched={}ms read_hash={}ms parse={}ms extract={}ms build={}ms",
                phases.sched.as_millis(),
                phases.read_hash.as_millis(),
                phases.parse.as_millis(),
                phases.extract.as_millis(),
                phases.build.as_millis(),
            );
        }

        (chunks, file_metadata)
    }

    fn build_chunk_reuse_map(&self, files: &[PathBuf]) -> ChunkReuseMap {
        let requested: HashSet<&Path> = files.iter().map(PathBuf::as_path).collect();
        let mut reuse_map: ChunkReuseMap = HashMap::new();

        for entry in &self.entries {
            if !requested.contains(entry.chunk.file.as_path()) {
                continue;
            }

            // `embed_text` is already persisted in the current on-disk format,
            // so refresh-time reuse can hash it in memory and confirm the exact
            // string without bumping `SEMANTIC_INDEX_VERSION` and forcing every
            // user through a full rebuild.
            let hash = blake3::hash(entry.chunk.embed_text.as_bytes());
            reuse_map
                .entry(entry.chunk.file.clone())
                .or_default()
                .entry(hash)
                .or_default()
                .push(ReusableEmbedding {
                    embed_text: entry.chunk.embed_text.clone(),
                    vector: entry.vector.clone(),
                });
        }

        reuse_map
    }

    fn extend_reuse_map_from_blob_store<R>(
        &self,
        project_root: &Path,
        files: impl IntoIterator<Item = PathBuf>,
        reuse_map: &mut ChunkReuseMap,
        reuse_blob: &mut R,
    ) where
        R: FnMut(&Path) -> Option<Vec<u8>>,
    {
        let Some(fingerprint) = self.fingerprint().map(SemanticIndexFingerprint::as_string) else {
            return;
        };
        let mut reused_files = 0usize;
        for file in files {
            let Some(payload) = reuse_blob(&file) else {
                continue;
            };
            match extend_reuse_map_from_semantic_blob(
                reuse_map,
                &file,
                &payload,
                &fingerprint,
                self.dimension,
            ) {
                Ok(()) => reused_files += 1,
                Err(error) => slog_warn!(
                    "semantic blob reuse rejected for {}: {}",
                    file.display(),
                    error
                ),
            }
        }
        if reused_files > 0 {
            slog_info!(
                "semantic refresh reused content-addressed vectors: root={} files={}",
                project_root.display(),
                reused_files
            );
        }
    }

    fn reusable_vector_for_chunk(
        reuse_map: &ChunkReuseMap,
        chunk: &SemanticChunk,
    ) -> Option<Vec<f32>> {
        let hash = blake3::hash(chunk.embed_text.as_bytes());
        reuse_map
            .get(&chunk.file)?
            .get(&hash)?
            .iter()
            .find(|candidate| candidate.embed_text == chunk.embed_text)
            .map(|candidate| candidate.vector.clone())
    }

    fn entries_for_chunks_with_reuse<F, P>(
        chunks: Vec<SemanticChunk>,
        reuse_map: &ChunkReuseMap,
        embed_fn: &mut F,
        max_batch_size: usize,
        initial_observed_dimension: Option<usize>,
        refresh_label: &str,
        progress: &mut P,
    ) -> Result<(Vec<EmbeddingEntry>, Option<usize>, usize), String>
    where
        F: FnMut(Vec<String>) -> Result<Vec<Vec<f32>>, String>,
        P: FnMut(usize, usize),
    {
        let total_chunks = chunks.len();
        progress(0, total_chunks);

        let mut entries_by_chunk: Vec<Option<EmbeddingEntry>> = vec![None; total_chunks];
        let mut misses: Vec<(usize, SemanticChunk)> = Vec::new();

        for (chunk_index, chunk) in chunks.into_iter().enumerate() {
            if let Some(vector) = Self::reusable_vector_for_chunk(reuse_map, &chunk) {
                entries_by_chunk[chunk_index] = Some(EmbeddingEntry::new(chunk, vector));
            } else {
                misses.push((chunk_index, chunk));
            }
        }

        let mut completed = total_chunks.saturating_sub(misses.len());
        if completed > 0 {
            progress(completed, total_chunks);
        }

        let batch_size = max_batch_size.max(1);
        let mut observed_dimension = initial_observed_dimension;
        let mut skipped_rows = 0usize;

        for batch_start in (0..misses.len()).step_by(batch_size) {
            let batch_end = (batch_start + batch_size).min(misses.len());
            let batch_texts: Vec<String> = misses[batch_start..batch_end]
                .iter()
                .map(|(_, chunk)| chunk.embed_text.clone())
                .collect();

            let rows = execute_build_embedding_batch(batch_texts, embed_fn)?;
            for (i, row) in rows.into_iter().enumerate() {
                let (chunk_index, mut chunk) = misses[batch_start + i].clone();
                match row {
                    BuildEmbeddingRow::Embedded {
                        embedded_text,
                        vector,
                    } => {
                        match observed_dimension {
                            None => observed_dimension = Some(vector.len()),
                            Some(expected) if vector.len() != expected => {
                                return Err(format!(
                                    "embedding dimension changed during {refresh_label}: cached index uses {expected}, new vectors use {}",
                                    vector.len()
                                ));
                            }
                            _ => {}
                        }
                        chunk.embed_text = embedded_text;
                        entries_by_chunk[chunk_index] = Some(EmbeddingEntry::new(chunk, vector));
                    }
                    BuildEmbeddingRow::Skipped {
                        embedded_text,
                        reason,
                    } => {
                        log_skipped_row_warning(&chunk, &embedded_text, &reason);
                        skipped_rows = skipped_rows.saturating_add(1);
                    }
                }
            }

            completed += batch_end - batch_start;
            progress(completed, total_chunks);
        }

        let entries = entries_by_chunk.into_iter().flatten().collect();

        Ok((entries, observed_dimension, skipped_rows))
    }

    fn build_from_chunks<F, P, C>(
        project_root: &Path,
        chunks: Vec<SemanticChunk>,
        file_metadata: HashMap<PathBuf, IndexedFileMetadata>,
        embed_fn: &mut F,
        max_batch_size: usize,
        mut progress: Option<&mut P>,
        should_continue: &mut C,
    ) -> Result<Self, String>
    where
        F: FnMut(Vec<String>) -> Result<Vec<Vec<f32>>, String>,
        P: FnMut(usize, usize),
        C: FnMut() -> bool,
    {
        debug_assert!(project_root.is_absolute());
        let total_chunks = chunks.len();

        if chunks.is_empty() {
            return Ok(Self {
                entries: Vec::new(),
                file_mtimes: file_metadata
                    .iter()
                    .map(|(path, metadata)| (path.clone(), metadata.mtime))
                    .collect(),
                file_sizes: file_metadata
                    .iter()
                    .map(|(path, metadata)| (path.clone(), metadata.size))
                    .collect(),
                any_missing_sizes: false,
                file_hashes: file_metadata
                    .into_iter()
                    .map(|(path, metadata)| (path, metadata.content_hash))
                    .collect(),
                dimension: DEFAULT_DIMENSION,
                fingerprint: None,
                project_root: project_root.to_path_buf(),
                deferred_files: HashSet::new(),
                shared_base: None,
                dirty_paths: Arc::new(Mutex::new(None)),
                persistence: Arc::new(Mutex::new(None)),
                last_append_read_bytes: Arc::new(AtomicUsize::new(0)),
                skipped_rows: 0,
                #[cfg(test)]
                removal_retain_passes: 0,
            });
        }

        let mut entries: Vec<EmbeddingEntry> = Vec::with_capacity(chunks.len());
        let mut expected_dimension: Option<usize> = None;
        let mut skipped_rows = 0usize;
        let mut completed_rows = 0usize;
        let batch_size = max_batch_size.max(1);
        let embed_started = std::time::Instant::now();
        let batch_count = total_chunks.div_ceil(batch_size);
        for (batch_index, batch_start) in (0..chunks.len()).step_by(batch_size).enumerate() {
            if !should_continue() {
                slog_info!(
                    "semantic embed superseded, stopping after {}/{} batches",
                    batch_index,
                    batch_count
                );
                return Err(format!(
                    "semantic build superseded after {batch_index}/{batch_count} batches"
                ));
            }
            let batch_end = (batch_start + batch_size).min(chunks.len());
            let batch_texts: Vec<String> = chunks[batch_start..batch_end]
                .iter()
                .map(|chunk| chunk.embed_text.clone())
                .collect();

            let rows = execute_build_embedding_batch(batch_texts, embed_fn)?;
            for (i, row) in rows.into_iter().enumerate() {
                let mut chunk = chunks[batch_start + i].clone();
                match row {
                    BuildEmbeddingRow::Embedded {
                        embedded_text,
                        vector,
                    } => {
                        match expected_dimension {
                            None => expected_dimension = Some(vector.len()),
                            Some(expected) if vector.len() != expected => {
                                return Err(format!(
                                    "embedding dimension changed across batches: expected {expected}, got {}",
                                    vector.len()
                                ));
                            }
                            _ => {}
                        }
                        chunk.embed_text = embedded_text;
                        entries.push(EmbeddingEntry::new(chunk, vector));
                    }
                    BuildEmbeddingRow::Skipped {
                        embedded_text,
                        reason,
                    } => {
                        log_skipped_row_warning(&chunk, &embedded_text, &reason);
                        skipped_rows = skipped_rows.saturating_add(1);
                    }
                }
            }

            completed_rows += batch_end - batch_start;
            if let Some(callback) = progress.as_mut() {
                callback(completed_rows, total_chunks);
            }
            if let Some(scope) = crate::logging::current_index_build() {
                if scope.plane == crate::logging::IndexPlane::Semantic {
                    crate::logging::log_index_event(
                        crate::logging::IndexEvent::from_scope(
                            crate::logging::IndexEventKind::BuildProgress,
                            &scope,
                        )
                        .field("stage", "embed")
                        .field("batch", batch_index + 1)
                        .field("total_batches", batch_count)
                        .field("chunks_done", completed_rows)
                        .field("completed", completed_rows)
                        .field("total", total_chunks)
                        .field("elapsed_ms", scope.elapsed_ms()),
                    );
                }
            }
            if (batch_index + 1) % 25 == 0 {
                slog_info!(
                    "semantic embed progress: batch {}/{} ({} / {} chunks)",
                    batch_index + 1,
                    batch_count,
                    completed_rows,
                    total_chunks
                );
            }
        }

        let embed_ms = embed_started.elapsed().as_millis();
        let rate = (total_chunks as u128 * 1000)
            .checked_div(embed_ms)
            .unwrap_or(0) as u64;
        slog_info!(
            "semantic embed: {} chunks in {} batches, {} ms ({} chunks/s), skipped_rows={}",
            total_chunks,
            batch_count,
            embed_ms,
            rate,
            skipped_rows,
        );

        let dimension = entries
            .first()
            .map(|entry| entry.vector.len())
            .unwrap_or(DEFAULT_DIMENSION);

        Ok(Self {
            entries,
            file_mtimes: file_metadata
                .iter()
                .map(|(path, metadata)| (path.clone(), metadata.mtime))
                .collect(),
            file_sizes: file_metadata
                .iter()
                .map(|(path, metadata)| (path.clone(), metadata.size))
                .collect(),
            any_missing_sizes: false,
            file_hashes: file_metadata
                .into_iter()
                .map(|(path, metadata)| (path, metadata.content_hash))
                .collect(),
            dimension,
            fingerprint: None,
            project_root: project_root.to_path_buf(),
            deferred_files: HashSet::new(),
            shared_base: None,
            dirty_paths: Arc::new(Mutex::new(None)),
            persistence: Arc::new(Mutex::new(None)),
            last_append_read_bytes: Arc::new(AtomicUsize::new(0)),
            skipped_rows,
            #[cfg(test)]
            removal_retain_passes: 0,
        })
    }

    /// Build the semantic index from a set of files using the provided embedding function.
    /// `embed_fn` takes a batch of texts and returns a batch of embedding vectors.
    pub fn build<F>(
        project_root: &Path,
        files: &[PathBuf],
        embed_fn: &mut F,
        max_batch_size: usize,
    ) -> Result<Self, String>
    where
        F: FnMut(Vec<String>) -> Result<Vec<Vec<f32>>, String>,
    {
        Self::build_with_caps(
            project_root,
            files,
            embed_fn,
            max_batch_size,
            EmbedTextCaps::default(),
        )
    }

    /// Build using explicitly resolved symbol-row caps.
    pub fn build_with_caps<F>(
        project_root: &Path,
        files: &[PathBuf],
        embed_fn: &mut F,
        max_batch_size: usize,
        embed_text_caps: EmbedTextCaps,
    ) -> Result<Self, String>
    where
        F: FnMut(Vec<String>) -> Result<Vec<Vec<f32>>, String>,
    {
        let (_guard, scope, mut failure_guard) = begin_semantic_index_build(project_root);
        let (chunks, file_mtimes) = Self::collect_chunks(project_root, files, embed_text_caps);
        let mut should_continue = || true;
        let result = Self::build_from_chunks(
            project_root,
            chunks,
            file_mtimes,
            embed_fn,
            max_batch_size,
            Option::<&mut fn(usize, usize)>::None,
            &mut should_continue,
        );
        finish_semantic_index_build(&scope, &mut failure_guard, &result);
        result
    }

    /// Build the semantic index and report embedding progress using entry counts.
    pub fn build_with_progress<F, P>(
        project_root: &Path,
        files: &[PathBuf],
        embed_fn: &mut F,
        max_batch_size: usize,
        progress: &mut P,
    ) -> Result<Self, String>
    where
        F: FnMut(Vec<String>) -> Result<Vec<Vec<f32>>, String>,
        P: FnMut(usize, usize),
    {
        let (_guard, scope, mut failure_guard) = begin_semantic_index_build(project_root);
        let (chunks, file_mtimes) =
            Self::collect_chunks(project_root, files, EmbedTextCaps::default());
        let total_chunks = chunks.len();
        progress(0, total_chunks);
        let mut should_continue = || true;
        let result = Self::build_from_chunks(
            project_root,
            chunks,
            file_mtimes,
            embed_fn,
            max_batch_size,
            Some(progress),
            &mut should_continue,
        );
        finish_semantic_index_build(&scope, &mut failure_guard, &result);
        result
    }

    /// Build the semantic index while checking cancellation before every embed
    /// batch. A batch already in flight is allowed to finish, then the partial
    /// result is discarded before the next request can start.
    pub fn build_with_progress_and_cancellation<F, P, C>(
        project_root: &Path,
        files: &[PathBuf],
        embed_fn: &mut F,
        max_batch_size: usize,
        progress: &mut P,
        should_continue: &mut C,
    ) -> Result<Self, String>
    where
        F: FnMut(Vec<String>) -> Result<Vec<Vec<f32>>, String>,
        P: FnMut(usize, usize),
        C: FnMut() -> bool,
    {
        Self::build_with_progress_and_cancellation_caps(
            project_root,
            files,
            embed_fn,
            max_batch_size,
            EmbedTextCaps::default(),
            progress,
            should_continue,
        )
    }

    pub fn build_with_progress_and_cancellation_caps<F, P, C>(
        project_root: &Path,
        files: &[PathBuf],
        embed_fn: &mut F,
        max_batch_size: usize,
        embed_text_caps: EmbedTextCaps,
        progress: &mut P,
        should_continue: &mut C,
    ) -> Result<Self, String>
    where
        F: FnMut(Vec<String>) -> Result<Vec<Vec<f32>>, String>,
        P: FnMut(usize, usize),
        C: FnMut() -> bool,
    {
        let (_guard, scope, mut failure_guard) = begin_semantic_index_build(project_root);
        let (chunks, file_mtimes) = Self::collect_chunks(project_root, files, embed_text_caps);
        let total_chunks = chunks.len();
        progress(0, total_chunks);
        let result = Self::build_from_chunks(
            project_root,
            chunks,
            file_mtimes,
            embed_fn,
            max_batch_size,
            Some(progress),
            should_continue,
        );
        finish_semantic_index_build(&scope, &mut failure_guard, &result);
        result
    }

    /// Incrementally refresh entries for changed/new files only, preserving cached
    /// embeddings for unchanged files. Used when loading the index from disk and
    /// finding that a small fraction of files have moved on, deleted, or appeared.
    ///
    /// Returns `RefreshSummary` describing what changed. On success, `self` is
    /// mutated in place and remains a valid index.
    ///
    /// `current_files` is the full set of files the project considers indexable
    /// (typically `walk_project_files(...)`). Files in the cache that are no
    /// longer in this set are treated as deleted.
    pub fn refresh_stale_files<F, P>(
        &mut self,
        project_root: &Path,
        current_files: &[PathBuf],
        embed_fn: &mut F,
        max_batch_size: usize,
        progress: &mut P,
    ) -> Result<RefreshSummary, String>
    where
        F: FnMut(Vec<String>) -> Result<Vec<Vec<f32>>, String>,
        P: FnMut(usize, usize),
    {
        self.refresh_stale_files_with_strategy(
            project_root,
            current_files,
            embed_fn,
            max_batch_size,
            progress,
            cache_freshness::VerifyStrategy::Strict,
        )
    }

    pub(crate) fn refresh_stale_files_with_strategy<F, P>(
        &mut self,
        project_root: &Path,
        current_files: &[PathBuf],
        embed_fn: &mut F,
        max_batch_size: usize,
        progress: &mut P,
        verify_strategy: cache_freshness::VerifyStrategy,
    ) -> Result<RefreshSummary, String>
    where
        F: FnMut(Vec<String>) -> Result<Vec<Vec<f32>>, String>,
        P: FnMut(usize, usize),
    {
        self.refresh_stale_files_with_strategy_and_blob_reuse(
            project_root,
            current_files,
            embed_fn,
            max_batch_size,
            progress,
            verify_strategy,
            &mut |_| None,
            None,
        )
    }

    pub(crate) fn refresh_stale_files_with_strategy_and_blob_reuse<F, P, R>(
        &mut self,
        project_root: &Path,
        current_files: &[PathBuf],
        embed_fn: &mut F,
        max_batch_size: usize,
        progress: &mut P,
        verify_strategy: cache_freshness::VerifyStrategy,
        reuse_blob: &mut R,
        mut recovery_paths: Option<&mut Vec<PathBuf>>,
    ) -> Result<RefreshSummary, String>
    where
        F: FnMut(Vec<String>) -> Result<Vec<Vec<f32>>, String>,
        P: FnMut(usize, usize),
        R: FnMut(&Path) -> Option<Vec<u8>>,
    {
        self.materialize_shared_base();
        self.backfill_missing_file_sizes();

        // 1. Bucket files into deleted / changed / added.
        let current_set: HashSet<&Path> = current_files.iter().map(PathBuf::as_path).collect();
        self.deferred_files
            .retain(|path| current_set.contains(path.as_path()));
        let total_processed = current_set.len() + self.file_mtimes.len()
            - self
                .file_mtimes
                .keys()
                .filter(|path| current_set.contains(path.as_path()))
                .count();

        // Files in cache that disappeared from disk OR are no longer in the
        // walked set. Both cases need their entries dropped.
        enum IndexedFileCheck {
            Deleted(PathBuf),
            MissingMetadata(PathBuf),
            Verified(PathBuf, FreshnessVerdict),
        }

        let mut deleted: Vec<PathBuf> = Vec::new();
        let mut changed: Vec<PathBuf> = Vec::new();
        let indexed_paths: Vec<PathBuf> = self.file_mtimes.keys().cloned().collect();
        let mut checks: Vec<Option<IndexedFileCheck>> = Vec::with_capacity(indexed_paths.len());
        let mut strict_verify_inputs: Vec<(usize, PathBuf, FileFreshness)> = Vec::new();

        for indexed_path in indexed_paths {
            let check_index = checks.len();
            if !current_set.contains(indexed_path.as_path()) {
                checks.push(Some(IndexedFileCheck::Deleted(indexed_path)));
                continue;
            }
            let cached = match (
                self.file_mtimes.get(&indexed_path),
                self.file_sizes.get(&indexed_path),
                self.file_hashes.get(&indexed_path),
            ) {
                (Some(mtime), Some(size), Some(hash)) => Some(FileFreshness {
                    mtime: *mtime,
                    size: *size,
                    content_hash: *hash,
                }),
                _ => None,
            };
            if let Some(freshness) = cached {
                strict_verify_inputs.push((check_index, indexed_path, freshness));
                checks.push(None);
            } else {
                checks.push(Some(IndexedFileCheck::MissingMetadata(indexed_path)));
            }
        }

        let verified = match verify_strategy {
            cache_freshness::VerifyStrategy::StatFirst => cache_freshness::verify_files_bounded(
                strict_verify_inputs,
                cache_freshness::VerifyStrategy::StatFirst,
            ),
            cache_freshness::VerifyStrategy::Strict => {
                cache_freshness::verify_files_strict_bounded(strict_verify_inputs)
            }
        };
        for (check_index, path, verdict) in verified {
            checks[check_index] = Some(IndexedFileCheck::Verified(path, verdict));
        }

        for check in checks {
            match check.expect("freshness check should be populated") {
                IndexedFileCheck::Deleted(path) => deleted.push(path),
                IndexedFileCheck::MissingMetadata(path) => changed.push(path),
                IndexedFileCheck::Verified(_path, FreshnessVerdict::HotFresh) => {}
                IndexedFileCheck::Verified(
                    path,
                    FreshnessVerdict::ContentFresh {
                        new_mtime,
                        new_size,
                    },
                ) => {
                    self.file_mtimes.insert(path.clone(), new_mtime);
                    self.file_sizes.insert(path, new_size);
                }
                IndexedFileCheck::Verified(
                    path,
                    FreshnessVerdict::Stale | FreshnessVerdict::Deleted,
                ) => {
                    changed.push(path);
                }
            }
        }

        // Files in walk that were never indexed.
        let mut added: Vec<PathBuf> = Vec::new();
        for path in current_files {
            if !self.file_mtimes.contains_key(path) {
                added.push(path.clone());
            }
        }

        // Fast path: nothing to do.
        if deleted.is_empty() && changed.is_empty() && added.is_empty() {
            progress(0, 0);
            return Ok(RefreshSummary {
                total_processed,
                ..RefreshSummary::default()
            });
        }

        // 2. Drop entries for deleted files immediately. Changed files are only
        //    replaced after successful re-extraction + embedding so transient
        //    read/parse errors keep the stale-but-valid cache entry.
        if !deleted.is_empty() {
            self.remove_indexed_files(&deleted);
        }

        // 3. Embed the changed + added set, if any.
        let mut to_embed: Vec<PathBuf> = Vec::with_capacity(changed.len() + added.len());
        to_embed.extend(changed.iter().cloned());
        to_embed.extend(added.iter().cloned());
        if let Some(paths) = recovery_paths.as_mut() {
            paths.clear();
            paths.extend(deleted.iter().cloned());
            paths.extend(to_embed.iter().cloned());
            paths.sort();
            paths.dedup();
        }

        if to_embed.is_empty() {
            // Only deletions happened.
            progress(0, 0);
            return Ok(RefreshSummary {
                changed: 0,
                added: 0,
                deleted: deleted.len(),
                total_processed,
            });
        }

        let mut reuse_map = self.build_chunk_reuse_map(&changed);
        let embed_text_caps = self
            .fingerprint
            .as_ref()
            .map(|fingerprint| fingerprint.embed_text_caps)
            .unwrap_or_default();
        let (chunks, fresh_metadata) =
            Self::collect_chunks(project_root, &to_embed, embed_text_caps);
        self.extend_reuse_map_from_blob_store(
            project_root,
            fresh_metadata.keys().cloned(),
            &mut reuse_map,
            reuse_blob,
        );
        let changed_set: HashSet<&Path> = changed.iter().map(PathBuf::as_path).collect();
        let vanished = to_embed
            .iter()
            .filter(|path| {
                changed_set.contains(path.as_path())
                    && !fresh_metadata.contains_key(*path)
                    && !path.exists()
            })
            .cloned()
            .collect::<Vec<_>>();
        if !vanished.is_empty() {
            self.remove_indexed_files(&vanished);
            deleted.extend(vanished);
        }

        if chunks.is_empty() {
            progress(0, 0);
            let successful_files: HashSet<PathBuf> = fresh_metadata.keys().cloned().collect();
            for file in &successful_files {
                self.deferred_files.remove(file);
            }
            if !successful_files.is_empty() {
                self.entries
                    .retain(|entry| !successful_files.contains(&entry.chunk.file));
            }
            let changed_count = changed
                .iter()
                .filter(|path| successful_files.contains(*path))
                .count();
            let added_count = added
                .iter()
                .filter(|path| successful_files.contains(*path))
                .count();
            for (file, metadata) in fresh_metadata {
                self.file_mtimes.insert(file.clone(), metadata.mtime);
                self.file_sizes.insert(file.clone(), metadata.size);
                self.file_hashes.insert(file.clone(), metadata.content_hash);
            }
            self.extend_dirty_paths(successful_files.iter().cloned());
            return Ok(RefreshSummary {
                changed: changed_count,
                added: added_count,
                deleted: deleted.len(),
                total_processed,
            });
        }

        // 4. Build the full replacement set, reusing cached vectors for chunks
        //    whose embed_text is unchanged and embedding only cache misses.
        let existing_dimension = if self.entries.is_empty() {
            None
        } else {
            Some(self.dimension)
        };
        let (new_entries, observed_dimension, skipped_rows) = Self::entries_for_chunks_with_reuse(
            chunks,
            &reuse_map,
            embed_fn,
            max_batch_size,
            existing_dimension,
            "incremental refresh",
            progress,
        )?;
        self.skipped_rows = self.skipped_rows.saturating_add(skipped_rows);

        let successful_files: HashSet<PathBuf> = fresh_metadata.keys().cloned().collect();
        for file in &successful_files {
            self.deferred_files.remove(file);
        }
        if !successful_files.is_empty() {
            self.entries
                .retain(|entry| !successful_files.contains(&entry.chunk.file));
        }

        self.entries.extend(new_entries);
        for (file, metadata) in fresh_metadata {
            self.file_mtimes.insert(file.clone(), metadata.mtime);
            self.file_sizes.insert(file.clone(), metadata.size);
            self.file_hashes.insert(file, metadata.content_hash);
        }
        if let Some(dim) = observed_dimension {
            self.dimension = dim;
        }
        self.extend_dirty_paths(successful_files.iter().cloned());

        Ok(RefreshSummary {
            changed: changed
                .iter()
                .filter(|path| successful_files.contains(*path))
                .count(),
            added: added
                .iter()
                .filter(|path| successful_files.contains(*path))
                .count(),
            deleted: deleted.len(),
            total_processed,
        })
    }

    /// Refresh exactly the files invalidated by the live watcher, without
    /// treating the provided path list as the whole project. This is the
    /// watcher-side counterpart to `refresh_stale_files`: it drops any stale
    /// entries for the requested paths from this in-memory index, re-extracts
    /// whatever still exists on disk, embeds those chunks, and returns the
    /// delta needed for another in-memory index to apply the same update.
    pub fn refresh_invalidated_files<F, P>(
        &mut self,
        project_root: &Path,
        paths: &[PathBuf],
        embed_fn: &mut F,
        max_batch_size: usize,
        max_files: usize,
        progress: &mut P,
    ) -> Result<InvalidatedFilesRefresh, String>
    where
        F: FnMut(Vec<String>) -> Result<Vec<Vec<f32>>, String>,
        P: FnMut(usize, usize),
    {
        self.refresh_invalidated_files_with_blob_reuse(
            project_root,
            paths,
            embed_fn,
            max_batch_size,
            max_files,
            progress,
            &mut |_| None,
        )
    }

    pub(crate) fn refresh_invalidated_files_with_blob_reuse<F, P, R>(
        &mut self,
        project_root: &Path,
        paths: &[PathBuf],
        embed_fn: &mut F,
        max_batch_size: usize,
        max_files: usize,
        progress: &mut P,
        reuse_blob: &mut R,
    ) -> Result<InvalidatedFilesRefresh, String>
    where
        F: FnMut(Vec<String>) -> Result<Vec<Vec<f32>>, String>,
        P: FnMut(usize, usize),
        R: FnMut(&Path) -> Option<Vec<u8>>,
    {
        self.materialize_shared_base();
        self.backfill_missing_file_sizes();

        self.deferred_files.retain(|path| path.exists());
        let mut requested_paths = paths.to_vec();
        requested_paths.extend(self.deferred_files.iter().cloned());
        requested_paths.sort();
        requested_paths.dedup();
        let total_processed = requested_paths.len();

        if requested_paths.is_empty() {
            progress(0, 0);
            return Ok(InvalidatedFilesRefresh {
                summary: RefreshSummary {
                    total_processed,
                    ..RefreshSummary::default()
                },
                ..InvalidatedFilesRefresh::default()
            });
        }

        let previously_indexed: HashSet<PathBuf> = requested_paths
            .iter()
            .filter(|path| self.file_mtimes.contains_key(*path))
            .cloned()
            .collect();
        let mut reuse_map = self.build_chunk_reuse_map(&requested_paths);

        // The watcher path has already invalidated these files in the request
        // thread's live index. Mirror that behavior here before inserting any
        // fresh chunks so parse/read failures do not resurrect stale entries.
        self.remove_indexed_files(&requested_paths);

        let existing_paths = requested_paths
            .iter()
            .filter(|path| path.exists())
            .cloned()
            .collect::<Vec<_>>();
        let deleted = requested_paths
            .iter()
            .filter(|path| !path.exists() && previously_indexed.contains(path.as_path()))
            .count();

        if existing_paths.is_empty() {
            for path in &requested_paths {
                if !path.exists() {
                    self.deferred_files.remove(path);
                }
            }
            progress(0, 0);
            return Ok(InvalidatedFilesRefresh {
                completed_paths: requested_paths,
                summary: RefreshSummary {
                    deleted,
                    total_processed,
                    ..RefreshSummary::default()
                },
                ..InvalidatedFilesRefresh::default()
            });
        }

        let embed_text_caps = self
            .fingerprint
            .as_ref()
            .map(|fingerprint| fingerprint.embed_text_caps)
            .unwrap_or_default();
        let (mut chunks, mut fresh_metadata) =
            Self::collect_chunks(project_root, &existing_paths, embed_text_caps);
        self.extend_reuse_map_from_blob_store(
            project_root,
            fresh_metadata.keys().cloned(),
            &mut reuse_map,
            reuse_blob,
        );

        let retained_file_count = self.file_mtimes.len();
        let changed_successful_count = existing_paths
            .iter()
            .filter(|path| {
                previously_indexed.contains(path.as_path()) && fresh_metadata.contains_key(*path)
            })
            .count();
        let available_new_files =
            max_files.saturating_sub(retained_file_count.saturating_add(changed_successful_count));
        let new_successful_files = existing_paths
            .iter()
            .filter(|path| {
                !previously_indexed.contains(path.as_path()) && fresh_metadata.contains_key(*path)
            })
            .cloned()
            .collect::<Vec<_>>();
        if new_successful_files.len() > available_new_files {
            let allowed_new_files = new_successful_files
                .iter()
                .take(available_new_files)
                .cloned()
                .collect::<HashSet<_>>();
            let deferred_new_files = new_successful_files
                .into_iter()
                .filter(|path| !allowed_new_files.contains(path))
                .collect::<HashSet<_>>();

            fresh_metadata.retain(|file, _| {
                previously_indexed.contains(file.as_path()) || allowed_new_files.contains(file)
            });
            chunks.retain(|chunk| !deferred_new_files.contains(&chunk.file));

            if !deferred_new_files.is_empty() {
                for path in &deferred_new_files {
                    self.deferred_files.insert(path.clone());
                }
                slog_warn!(
                    "semantic refresh deferred {} new file(s): indexed-file cap {} is reached",
                    deferred_new_files.len(),
                    max_files
                );
            }
        }

        let successful_files: HashSet<PathBuf> = fresh_metadata.keys().cloned().collect();
        for file in &successful_files {
            self.deferred_files.remove(file);
        }
        let changed = successful_files
            .iter()
            .filter(|path| previously_indexed.contains(path.as_path()))
            .count();
        let added = successful_files.len().saturating_sub(changed);
        let mut updated_metadata = Vec::with_capacity(fresh_metadata.len());

        if chunks.is_empty() {
            progress(0, 0);
            for (file, metadata) in fresh_metadata {
                let freshness = FileFreshness {
                    mtime: metadata.mtime,
                    size: metadata.size,
                    content_hash: metadata.content_hash,
                };
                self.file_mtimes.insert(file.clone(), freshness.mtime);
                self.file_sizes.insert(file.clone(), freshness.size);
                self.file_hashes
                    .insert(file.clone(), freshness.content_hash);
                updated_metadata.push((file, freshness));
            }

            return Ok(InvalidatedFilesRefresh {
                updated_metadata,
                completed_paths: requested_paths,
                summary: RefreshSummary {
                    changed,
                    added,
                    deleted,
                    total_processed,
                },
                ..InvalidatedFilesRefresh::default()
            });
        }

        let initial_observed_dimension = if self.entries.is_empty() && previously_indexed.is_empty()
        {
            None
        } else {
            Some(self.dimension)
        };
        let (new_entries, observed_dimension, skipped_rows) = Self::entries_for_chunks_with_reuse(
            chunks,
            &reuse_map,
            embed_fn,
            max_batch_size,
            initial_observed_dimension,
            "invalidated-file refresh",
            progress,
        )?;
        self.skipped_rows = self.skipped_rows.saturating_add(skipped_rows);

        let added_entries = new_entries.clone();
        self.entries.extend(new_entries);
        for (file, metadata) in fresh_metadata {
            let freshness = FileFreshness {
                mtime: metadata.mtime,
                size: metadata.size,
                content_hash: metadata.content_hash,
            };
            self.file_mtimes.insert(file.clone(), freshness.mtime);
            self.file_sizes.insert(file.clone(), freshness.size);
            self.file_hashes
                .insert(file.clone(), freshness.content_hash);
            updated_metadata.push((file, freshness));
        }
        if let Some(dim) = observed_dimension {
            self.dimension = dim;
        }

        Ok(InvalidatedFilesRefresh {
            added_entries,
            updated_metadata,
            completed_paths: requested_paths,
            summary: RefreshSummary {
                changed,
                added,
                deleted,
                total_processed,
            },
        })
    }

    pub fn apply_refresh_update(
        &mut self,
        added_entries: Vec<EmbeddingEntry>,
        updated_metadata: Vec<(PathBuf, FileFreshness)>,
        completed_paths: &[PathBuf],
    ) {
        self.materialize_shared_base();
        // `added_entries` is the complete replacement set for completed paths:
        // freshly embedded misses plus reused chunks carrying refreshed metadata.
        // Removing first is safe only because producers include both kinds.
        self.remove_indexed_files(completed_paths);

        let observed_dimension = added_entries.first().map(|entry| entry.vector.len());
        self.entries.extend(added_entries);
        for (file, freshness) in updated_metadata {
            self.file_mtimes.insert(file.clone(), freshness.mtime);
            self.file_sizes.insert(file.clone(), freshness.size);
            self.file_hashes.insert(file, freshness.content_hash);
        }
        if let Some(dim) = observed_dimension {
            self.dimension = dim;
        }
    }

    fn dirty_paths_snapshot(&self) -> Option<BTreeSet<PathBuf>> {
        self.dirty_paths
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn set_dirty_paths(&self, paths: Option<BTreeSet<PathBuf>>) {
        *self
            .dirty_paths
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = paths;
    }

    fn persistence_snapshot(&self) -> Option<SemanticPersistenceState> {
        *self
            .persistence
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn set_persistence(&self, state: Option<SemanticPersistenceState>) {
        *self
            .persistence
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = state;
    }

    fn extend_dirty_paths(&self, paths: impl IntoIterator<Item = PathBuf>) {
        if let Some(dirty_paths) = self
            .dirty_paths
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_mut()
        {
            dirty_paths.extend(paths);
        }
    }

    fn mark_all_dirty(&self) {
        let mut paths = BTreeSet::new();
        paths.extend(self.file_mtimes.keys().cloned());
        paths.extend(self.entries.iter().map(|entry| entry.chunk.file.clone()));
        self.set_dirty_paths(Some(paths));
    }

    fn remove_indexed_file_keys(
        &mut self,
        entry_files: &HashSet<PathBuf>,
        metadata_files: &[PathBuf],
    ) {
        #[cfg(test)]
        {
            self.removal_retain_passes += 1;
        }
        self.entries
            .retain(|entry| !entry_files.contains(&entry.chunk.file));
        for path in metadata_files {
            self.file_mtimes.remove(path);
            self.file_sizes.remove(path);
            self.file_hashes.remove(path);
        }
        self.extend_dirty_paths(metadata_files.iter().cloned());
    }

    fn remove_indexed_files(&mut self, files: &[PathBuf]) {
        let deleted_set = files.iter().cloned().collect();
        self.remove_indexed_file_keys(&deleted_set, files);
    }

    /// Search the index with a query embedding, returning top-K results sorted by relevance.
    pub fn search(&self, query_vector: &[f32], top_k: usize) -> Vec<SemanticResult> {
        self.search_filtered(query_vector, top_k, |_| true)
    }

    /// Search only entries whose resolved source path satisfies `include`.
    ///
    /// Filtering before top-K selection prevents excluded files from consuming the
    /// bounded candidate window and hiding lower-ranked eligible results.
    pub(crate) fn search_filtered<F>(
        &self,
        query_vector: &[f32],
        top_k: usize,
        include: F,
    ) -> Vec<SemanticResult>
    where
        F: Fn(&Path) -> bool,
    {
        let (entries, dimension) = self
            .shared_base
            .as_ref()
            .map(|base| (base.entries.as_slice(), base.dimension))
            .unwrap_or_else(|| (self.entries.as_slice(), self.dimension));
        if entries.is_empty() || query_vector.len() != dimension {
            return Vec::new();
        }

        // Query norms are shared by every entry; entry norms are cached because
        // remote embedding backends may return non-normalized vectors.
        let query_norm = vector_norm(query_vector);
        let cancellation = crate::executor::current_job_cancellation();
        let mut scored: Vec<(f32, usize)> = Vec::with_capacity(entries.len());
        for (i, entry) in entries.iter().enumerate() {
            if i % 64 == 0
                && cancellation
                    .as_ref()
                    .is_some_and(|token| token.cancel_requested_before_commit())
            {
                break;
            }
            let included = if self.shared_base.is_some() {
                include(&self.project_root.join(&entry.chunk.file))
            } else {
                include(&entry.chunk.file)
            };
            if !included {
                continue;
            }

            let dot = if query_vector.len() == entry.vector.len() {
                dot_product(query_vector, &entry.vector)
            } else {
                0.0
            };
            let denom = query_norm * entry.norm;
            let mut score = if denom == 0.0 { 0.0 } else { dot / denom };
            if entry.chunk.exported {
                score *= 1.1;
            }
            scored.push((score, i));
        }

        let keep = top_k.min(scored.len());
        if keep == 0 {
            return Vec::new();
        }

        if keep < scored.len() {
            scored.select_nth_unstable_by(keep, semantic_score_order);
            scored.truncate(keep);
        }
        scored.sort_by(semantic_score_order);

        scored
            .into_iter()
            // Keep the selected best-first slice mapped without reintroducing the
            // old `> 0.0` floor: top_k has already been selected, and zero-score
            // tail entries remain observable when requested.
            .map(|(score, idx)| {
                let entry = &entries[idx];
                SemanticResult {
                    file: if self.shared_base.is_some() {
                        self.project_root.join(&entry.chunk.file)
                    } else {
                        entry.chunk.file.clone()
                    },
                    name: entry.chunk.name.clone(),
                    qualified_name: entry.chunk.qualified_name.clone(),
                    kind: entry.chunk.kind.clone(),
                    start_line: entry.chunk.start_line,
                    end_line: entry.chunk.end_line,
                    exported: entry.chunk.exported,
                    snippet: entry.chunk.snippet.clone(),
                    score,
                    rank_score: score,
                    cap_protected: false,
                    source: "semantic",
                }
            })
            .collect()
    }

    /// Number of indexed entries
    pub fn len(&self) -> usize {
        self.entry_count()
    }

    /// Check if a file needs re-indexing based on mtime/size
    pub fn is_file_stale(&self, file: &Path) -> bool {
        let relative;
        let (file_mtimes, file_sizes, file_hashes, lookup) = if let Some(base) = &self.shared_base {
            relative = file
                .strip_prefix(&self.project_root)
                .unwrap_or(file)
                .to_path_buf();
            (
                &base.file_mtimes,
                &base.file_sizes,
                &base.file_hashes,
                relative.as_path(),
            )
        } else {
            (&self.file_mtimes, &self.file_sizes, &self.file_hashes, file)
        };
        let Some(stored_mtime) = file_mtimes.get(lookup) else {
            return true;
        };
        let Some(stored_size) = file_sizes.get(lookup) else {
            return true;
        };
        let Some(stored_hash) = file_hashes.get(lookup) else {
            return true;
        };
        let cached = FileFreshness {
            mtime: *stored_mtime,
            size: *stored_size,
            content_hash: *stored_hash,
        };
        match cache_freshness::verify_file_strict(file, &cached) {
            FreshnessVerdict::HotFresh => false,
            FreshnessVerdict::ContentFresh { .. } => false,
            FreshnessVerdict::Stale | FreshnessVerdict::Deleted => true,
        }
    }

    fn backfill_missing_file_sizes(&mut self) {
        if !self.any_missing_sizes {
            return;
        }

        for path in self.file_mtimes.keys() {
            if self.file_sizes.contains_key(path) {
                continue;
            }
            if let Ok(metadata) = fs::metadata(path) {
                self.file_sizes.insert(path.clone(), metadata.len());
                if let Ok(Some(hash)) = cache_freshness::hash_file_if_small(path, metadata.len()) {
                    self.file_hashes.insert(path.clone(), hash);
                }
            }
        }
        self.any_missing_sizes = self
            .file_mtimes
            .keys()
            .any(|path| !self.file_sizes.contains_key(path));
    }

    /// Remove entries for a specific file.
    pub fn remove_file(&mut self, file: &Path) {
        self.invalidate_file(file);
    }

    pub fn invalidate_file(&mut self, file: &Path) {
        let file = file.to_path_buf();
        self.invalidate_files(std::slice::from_ref(&file));
    }

    pub fn invalidate_files(&mut self, files: &[PathBuf]) {
        if files.is_empty() {
            return;
        }
        self.materialize_shared_base();

        // Watchers may report a symlinked spelling while persisted metadata uses
        // the canonical spelling (or vice versa), so both keys must be removed.
        let mut invalidated = HashSet::with_capacity(files.len().saturating_mul(2));
        let mut metadata_keys = Vec::with_capacity(files.len().saturating_mul(2));
        for file in files {
            metadata_keys.push(file.clone());
            invalidated.insert(file.clone());
            let canonical = canonicalize_existing_or_deleted_path(file);
            if canonical != *file {
                metadata_keys.push(canonical.clone());
                invalidated.insert(canonical);
            }
        }
        self.remove_indexed_file_keys(&invalidated, &metadata_keys);
    }

    #[cfg(test)]
    pub(crate) fn removal_retain_passes_for_test(&self) -> usize {
        self.removal_retain_passes
    }

    #[cfg(test)]
    pub(crate) fn uses_shared_base_for_test(&self) -> bool {
        self.shared_base.is_some()
    }

    /// Get the embedding dimension
    pub fn dimension(&self) -> usize {
        self.shared_base
            .as_ref()
            .map(|base| base.dimension)
            .unwrap_or(self.dimension)
    }

    pub fn fingerprint(&self) -> Option<&SemanticIndexFingerprint> {
        self.shared_base
            .as_ref()
            .and_then(|base| base.fingerprint.as_ref())
            .or(self.fingerprint.as_ref())
    }

    pub fn backend_label(&self) -> Option<&str> {
        self.fingerprint().map(|f| f.backend.as_str())
    }

    pub fn model_label(&self) -> Option<&str> {
        self.fingerprint().map(|f| f.model.as_str())
    }

    pub fn set_fingerprint(&mut self, fingerprint: SemanticIndexFingerprint) {
        self.materialize_shared_base();
        self.fingerprint = Some(fingerprint);
    }

    fn scan_artifact_for_append(
        data_path: &Path,
        expected: SemanticPersistenceState,
        expected_fingerprint: &str,
        expected_dimension: usize,
    ) -> Result<SemanticArtifactLayout, String> {
        let mut file = fs::File::open(data_path).map_err(|error| error.to_string())?;
        let identity = semantic_artifact_identity(data_path)
            .ok_or_else(|| "semantic artifact identity unavailable".to_string())?;
        if identity != expected.identity {
            return Err("semantic artifact changed since it was loaded".to_string());
        }
        let file_len = usize::try_from(identity.bytes)
            .map_err(|_| "semantic artifact is too large for this platform".to_string())?;
        if expected.base_bytes < HEADER_BYTES_V2 || expected.base_bytes > file_len {
            return Err("persisted semantic base boundary is invalid".to_string());
        }

        let mut fixed = [0_u8; HEADER_BYTES_V2];
        file.read_exact(&mut fixed)
            .map_err(|error| error.to_string())?;
        if fixed[0] != SEMANTIC_INDEX_VERSION_V6 && fixed[0] != SEMANTIC_INDEX_VERSION_V7 {
            return Err(format!(
                "unsupported on-disk semantic version: {}",
                fixed[0]
            ));
        }
        let dimension = u32::from_le_bytes(fixed[1..5].try_into().unwrap()) as usize;
        if dimension != expected_dimension {
            return Err("semantic artifact dimension changed".to_string());
        }
        let fingerprint_len = u32::from_le_bytes(fixed[9..13].try_into().unwrap()) as usize;
        if fingerprint_len > 64 * 1024 {
            return Err("semantic artifact fingerprint is oversized".to_string());
        }
        let mut fingerprint = vec![0_u8; fingerprint_len];
        file.read_exact(&mut fingerprint)
            .map_err(|error| error.to_string())?;
        if fingerprint != expected_fingerprint.as_bytes() {
            return Err("semantic artifact fingerprint changed".to_string());
        }

        file.seek(SeekFrom::Start(expected.base_bytes as u64))
            .map_err(|error| error.to_string())?;
        let mut valid_bytes = expected.base_bytes;
        let mut segment_count = 0usize;
        let mut torn_tail = false;
        let mut bytes_read = HEADER_BYTES_V2.saturating_add(fingerprint_len);
        while valid_bytes < file_len {
            let remaining = file_len.saturating_sub(valid_bytes);
            if remaining < SEMANTIC_SEGMENT_FRAME_HEADER_BYTES {
                torn_tail = true;
                break;
            }
            let mut header = [0_u8; SEMANTIC_SEGMENT_FRAME_HEADER_BYTES];
            file.read_exact(&mut header)
                .map_err(|error| error.to_string())?;
            bytes_read = bytes_read.saturating_add(header.len());
            if &header[..8] != SEMANTIC_SEGMENT_MAGIC {
                torn_tail = true;
                break;
            }
            let payload_len =
                usize::try_from(u64::from_le_bytes(header[8..16].try_into().unwrap()))
                    .map_err(|_| "semantic segment length exceeds this platform".to_string())?;
            let frame_len = SEMANTIC_SEGMENT_FRAME_HEADER_BYTES
                .checked_add(payload_len)
                .ok_or_else(|| "semantic segment frame length overflow".to_string())?;
            if frame_len > remaining {
                torn_tail = true;
                break;
            }
            let frame_end = valid_bytes.saturating_add(frame_len);
            if frame_end == file_len {
                let mut payload = vec![0_u8; payload_len];
                file.read_exact(&mut payload)
                    .map_err(|error| error.to_string())?;
                bytes_read = bytes_read.saturating_add(payload_len);
                if blake3::hash(&payload).as_bytes()
                    != &header[16..SEMANTIC_SEGMENT_FRAME_HEADER_BYTES]
                {
                    torn_tail = true;
                    break;
                }
            } else {
                file.seek(SeekFrom::Current(payload_len as i64))
                    .map_err(|error| error.to_string())?;
            }
            valid_bytes = frame_end;
            segment_count = segment_count.saturating_add(1);
        }

        Ok(SemanticArtifactLayout {
            identity,
            base_bytes: expected.base_bytes,
            valid_bytes,
            segment_count,
            segment_bytes: valid_bytes.saturating_sub(expected.base_bytes),
            torn_tail,
            bytes_read,
        })
    }

    fn persistence_state_from_loaded(
        data_path: &Path,
        loaded: &LoadedSemanticArtifact,
    ) -> Option<SemanticPersistenceState> {
        Some(SemanticPersistenceState {
            identity: semantic_artifact_identity(data_path)?,
            base_bytes: loaded.base_bytes,
            segment_count: loaded.segment_count,
            segment_bytes: loaded.segment_bytes,
            valid_bytes: loaded.valid_bytes,
        })
    }

    fn write_full_snapshot_at(
        &self,
        dir: &Path,
        data_path: &Path,
        pause_before_swap: bool,
    ) -> io::Result<usize> {
        let tmp_path = dir.join(format!(
            "semantic.bin.tmp.{}.{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or(Duration::ZERO)
                .as_nanos()
        ));
        let write_result = (|| -> io::Result<usize> {
            let file = fs::File::create(&tmp_path)?;
            let mut writer = BufWriter::new(file);
            let bytes_written = self.write_to_writer(&mut writer)?;
            writer.flush()?;
            writer.get_ref().sync_all()?;
            Ok(bytes_written)
        })();
        let bytes_written = match write_result {
            Ok(bytes_written) => bytes_written,
            Err(error) => {
                let _ = fs::remove_file(&tmp_path);
                return Err(error);
            }
        };

        #[cfg(debug_assertions)]
        if pause_before_swap {
            if let Some(ready) = env::var_os("AFT_TEST_SEMANTIC_COMPACTION_READY") {
                let ready = PathBuf::from(ready);
                fs::write(&ready, b"ready")?;
                let release = ready.with_extension("release");
                let started = Instant::now();
                while !release.is_file() {
                    if started.elapsed() >= Duration::from_secs(30) {
                        let _ = fs::remove_file(&tmp_path);
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "timed out waiting at semantic compaction swap test seam",
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
        #[cfg(not(debug_assertions))]
        let _ = pause_before_swap;

        if let Err(error) = crate::fs_lock::rename_over(&tmp_path, data_path) {
            let _ = fs::remove_file(&tmp_path);
            return Err(error);
        }
        crate::fs_lock::sync_parent(data_path);
        Ok(bytes_written)
    }

    fn persistence_identity_matches(&self, previous: &Self) -> bool {
        self.dimension == previous.dimension
            && self
                .fingerprint
                .as_ref()
                .map(SemanticIndexFingerprint::as_string)
                == previous
                    .fingerprint
                    .as_ref()
                    .map(SemanticIndexFingerprint::as_string)
    }

    fn delta_for_paths(&self, paths: &BTreeSet<PathBuf>) -> Self {
        Self {
            entries: self
                .entries
                .iter()
                .filter(|entry| paths.contains(&entry.chunk.file))
                .cloned()
                .collect(),
            file_mtimes: self
                .file_mtimes
                .iter()
                .filter(|(path, _)| paths.contains(*path))
                .map(|(path, value)| (path.clone(), *value))
                .collect(),
            file_sizes: self
                .file_sizes
                .iter()
                .filter(|(path, _)| paths.contains(*path))
                .map(|(path, value)| (path.clone(), *value))
                .collect(),
            any_missing_sizes: false,
            file_hashes: self
                .file_hashes
                .iter()
                .filter(|(path, _)| paths.contains(*path))
                .map(|(path, value)| (path.clone(), *value))
                .collect(),
            dimension: self.dimension,
            fingerprint: self.fingerprint.clone(),
            project_root: self.project_root.clone(),
            deferred_files: HashSet::new(),
            shared_base: None,
            dirty_paths: Arc::new(Mutex::new(None)),
            persistence: Arc::new(Mutex::new(None)),
            last_append_read_bytes: Arc::new(AtomicUsize::new(0)),
            skipped_rows: self.skipped_rows,
            #[cfg(test)]
            removal_retain_passes: 0,
        }
    }

    fn build_segment_frame(
        &self,
        sequence: u64,
        changed_paths: &BTreeSet<PathBuf>,
    ) -> Result<Vec<u8>, String> {
        let fingerprint = self
            .fingerprint
            .as_ref()
            .map(SemanticIndexFingerprint::as_string)
            .unwrap_or_default();
        let mut payload = Vec::new();
        payload.push(SEMANTIC_SEGMENT_VERSION);
        payload.extend_from_slice(&sequence.to_le_bytes());
        payload.extend_from_slice(&(fingerprint.len() as u32).to_le_bytes());
        payload.extend_from_slice(fingerprint.as_bytes());
        payload.extend_from_slice(&(self.dimension as u32).to_le_bytes());
        payload.extend_from_slice(&(changed_paths.len() as u32).to_le_bytes());
        for path in changed_paths {
            let relative = cache_relative_path(&self.project_root, path).ok_or_else(|| {
                format!(
                    "semantic segment tombstone escapes project root: {}",
                    path.display()
                )
            })?;
            let relative = relative.to_string_lossy();
            payload.extend_from_slice(&(relative.len() as u32).to_le_bytes());
            payload.extend_from_slice(relative.as_bytes());
        }

        let delta_bytes = self.delta_for_paths(changed_paths).to_bytes();
        payload.extend_from_slice(&(delta_bytes.len() as u64).to_le_bytes());
        payload.extend_from_slice(&delta_bytes);

        let checksum = blake3::hash(&payload);
        let mut frame = Vec::with_capacity(SEMANTIC_SEGMENT_FRAME_HEADER_BYTES + payload.len());
        frame.extend_from_slice(SEMANTIC_SEGMENT_MAGIC);
        frame.extend_from_slice(&(payload.len() as u64).to_le_bytes());
        frame.extend_from_slice(checksum.as_bytes());
        frame.extend_from_slice(&payload);
        Ok(frame)
    }

    fn append_segment_frame(data_path: &Path, frame: &[u8]) -> io::Result<()> {
        let mut file = OpenOptions::new().append(true).open(data_path)?;

        #[cfg(debug_assertions)]
        if let Some(ready) = env::var_os("AFT_TEST_SEMANTIC_SEGMENT_TEAR_READY") {
            let cut = (frame.len() / 2).max(SEMANTIC_SEGMENT_FRAME_HEADER_BYTES);
            file.write_all(&frame[..cut])?;
            file.sync_all()?;
            fs::write(ready, b"ready")?;
            loop {
                std::thread::sleep(Duration::from_secs(1));
            }
        }

        file.write_all(frame)?;
        file.sync_all()
    }

    fn compact_path_if_unchanged(
        dir: &Path,
        data_path: &Path,
        project_root: &Path,
        expected: SemanticArtifactIdentity,
    ) -> bool {
        let Ok(_lock) = acquire_semantic_persistence_lock(dir, expected.bytes) else {
            return false;
        };
        if semantic_artifact_identity(data_path) != Some(expected) {
            return false;
        }
        let loaded = match Self::load_artifact_path(data_path, project_root) {
            Ok(loaded) if !loaded.torn_tail && loaded.valid_bytes as u64 == expected.bytes => {
                loaded
            }
            Ok(_) => return false,
            Err(error) => {
                slog_warn!("failed to load semantic index for compaction: {}", error);
                return false;
            }
        };
        slog_info!(
            "semantic index compaction started: root=\"{}\" segments={} segment_bytes={}",
            project_root.display(),
            loaded.segment_count,
            loaded.segment_bytes
        );
        let started = Instant::now();
        match loaded.index.write_full_snapshot_at(dir, data_path, true) {
            Ok(bytes_written) => {
                crate::write_ledger::credit(
                    crate::write_ledger::Domain::SemanticCompaction,
                    project_root.display().to_string(),
                    bytes_written as u64,
                    0,
                );
                slog_info!(
                    "semantic index compaction finished: root=\"{}\" segments={} segment_bytes={} entries={} bytes={} elapsed_ms={}",
                    project_root.display(),
                    loaded.segment_count,
                    loaded.segment_bytes,
                    loaded.index.entries.len(),
                    bytes_written,
                    started.elapsed().as_millis()
                );
                true
            }
            Err(error) => {
                slog_warn!("failed to compact semantic index: {}", error);
                false
            }
        }
    }

    fn schedule_compaction_if_needed(
        &self,
        dir: &Path,
        data_path: &Path,
        layout: &SemanticArtifactLayout,
        appended_bytes: usize,
    ) {
        let segment_count = layout.segment_count.saturating_add(1);
        let segment_bytes = layout.segment_bytes.saturating_add(appended_bytes);
        let byte_bound_crossed = (segment_bytes as u64)
            > (layout.base_bytes as u64 / SEMANTIC_COMPACT_BYTE_RATIO_DENOMINATOR);
        if segment_count <= SEMANTIC_COMPACT_SEGMENT_LIMIT && !byte_bound_crossed {
            return;
        }
        let Some(expected) = semantic_artifact_identity(data_path) else {
            return;
        };
        slog_info!(
            "semantic index compaction scheduled: root=\"{}\" segments={} segment_bytes={}",
            self.project_root.display(),
            segment_count,
            segment_bytes
        );
        let project_root = self.project_root.clone();
        let dir = dir.to_path_buf();
        let data_path = data_path.to_path_buf();
        let _ = std::thread::Builder::new()
            .name("semantic-index-compaction".to_string())
            .spawn(move || {
                Self::compact_path_if_unchanged(&dir, &data_path, &project_root, expected);
            });
    }

    /// Write a cold base snapshot or append one checksummed file-replacement segment.
    /// A final partial segment is ignored (and truncated by an owning reader/writer),
    /// so SIGKILL during append leaves every previously committed refresh loadable.
    pub fn write_to_disk(&self, storage_dir: &Path, project_key: &str) -> bool {
        if self.shared_base.is_some() {
            let mut private = self.clone();
            private.materialize_shared_base();
            return private.write_to_disk(storage_dir, project_key);
        }
        let dir = storage_dir.join("semantic").join(project_key);
        let data_path = dir.join("semantic.bin");
        let access = crate::root_cache::ArtifactAccess::for_root(&self.project_root);
        if !access.allows_write(project_key, &data_path) {
            return false;
        }
        if let Err(error) = fs::create_dir_all(&dir) {
            slog_warn!("failed to create semantic cache dir: {}", error);
            return false;
        }
        let artifact_bytes = semantic_artifact_identity(&data_path)
            .map(|identity| identity.bytes)
            .unwrap_or_default();
        let _persistence_lock = match acquire_semantic_persistence_lock(&dir, artifact_bytes) {
            Ok(lock) => lock,
            Err(error) => {
                slog_warn!("failed to acquire semantic persistence lock: {}", error);
                return false;
            }
        };

        if data_path.is_file() {
            let fingerprint = self
                .fingerprint
                .as_ref()
                .map(SemanticIndexFingerprint::as_string)
                .unwrap_or_default();
            let layout = self.persistence_snapshot().and_then(|persistence| {
                match Self::scan_artifact_for_append(
                    &data_path,
                    persistence,
                    &fingerprint,
                    self.dimension,
                ) {
                    Ok(layout) => Some(layout),
                    Err(error) => {
                        slog_info!(
                            "semantic delta metadata unavailable ({}); using structural fallback",
                            error
                        );
                        None
                    }
                }
            });
            let (layout, changed_paths) = if let (Some(layout), Some(dirty_paths)) =
                (layout, self.dirty_paths_snapshot())
            {
                (layout, dirty_paths.clone())
            } else {
                match Self::load_artifact_path(&data_path, &self.project_root) {
                    Ok(loaded) if self.persistence_identity_matches(&loaded.index) => {
                        let changed_paths = semantic_changed_paths(&loaded.index, self);
                        let identity = match semantic_artifact_identity(&data_path) {
                            Some(identity) => identity,
                            None => return false,
                        };
                        let layout = SemanticArtifactLayout {
                            identity,
                            base_bytes: loaded.base_bytes,
                            valid_bytes: loaded.valid_bytes,
                            segment_count: loaded.segment_count,
                            segment_bytes: loaded.segment_bytes,
                            torn_tail: loaded.torn_tail,
                            bytes_read: identity.bytes as usize,
                        };
                        (layout, changed_paths)
                    }
                    Ok(_) => {
                        self.set_persistence(None);
                        self.mark_all_dirty();
                        return self
                            .write_full_snapshot_at(&dir, &data_path, false)
                            .is_ok_and(|bytes_written| {
                                let Some(identity) = semantic_artifact_identity(&data_path) else {
                                    return false;
                                };
                                self.set_persistence(Some(SemanticPersistenceState {
                                    identity,
                                    base_bytes: bytes_written,
                                    segment_count: 0,
                                    segment_bytes: 0,
                                    valid_bytes: bytes_written,
                                }));
                                self.set_dirty_paths(Some(BTreeSet::new()));
                                slog_info!(
                                    "semantic index persisted: {} entries, {:.1} KB",
                                    self.entries.len(),
                                    bytes_written as f64 / 1024.0
                                );
                                crate::write_ledger::credit(
                                    crate::write_ledger::Domain::SemanticCold,
                                    self.project_root.display().to_string(),
                                    bytes_written as u64,
                                    0,
                                );
                                true
                            });
                    }
                    Err(error) => {
                        slog_warn!(
                            "semantic index delta baseline unavailable ({}); replacing base snapshot",
                            error
                        );
                        self.set_persistence(None);
                        self.mark_all_dirty();
                        return self
                            .write_full_snapshot_at(&dir, &data_path, false)
                            .is_ok_and(|bytes_written| {
                                let Some(identity) = semantic_artifact_identity(&data_path) else {
                                    return false;
                                };
                                self.set_persistence(Some(SemanticPersistenceState {
                                    identity,
                                    base_bytes: bytes_written,
                                    segment_count: 0,
                                    segment_bytes: 0,
                                    valid_bytes: bytes_written,
                                }));
                                self.set_dirty_paths(Some(BTreeSet::new()));
                                crate::write_ledger::credit(
                                    crate::write_ledger::Domain::SemanticCold,
                                    self.project_root.display().to_string(),
                                    bytes_written as u64,
                                    0,
                                );
                                true
                            });
                    }
                }
            };

            self.last_append_read_bytes
                .store(layout.bytes_read, Ordering::Relaxed);
            if layout.torn_tail {
                match OpenOptions::new()
                    .write(true)
                    .open(&data_path)
                    .and_then(|file| {
                        file.set_len(layout.valid_bytes as u64)?;
                        file.sync_all()
                    }) {
                    Ok(()) => {}
                    Err(error) => {
                        slog_warn!("failed to truncate torn semantic segment: {}", error);
                        return false;
                    }
                }
            }
            if changed_paths.is_empty() {
                self.set_dirty_paths(Some(BTreeSet::new()));
                self.set_persistence(Some(SemanticPersistenceState {
                    identity: layout.identity,
                    base_bytes: layout.base_bytes,
                    segment_count: layout.segment_count,
                    segment_bytes: layout.segment_bytes,
                    valid_bytes: layout.valid_bytes,
                }));
                return true;
            }
            let frame = match self.build_segment_frame(
                layout.segment_count.saturating_add(1) as u64,
                &changed_paths,
            ) {
                Ok(frame) => frame,
                Err(error) => {
                    slog_warn!("failed to encode semantic delta: {}", error);
                    return false;
                }
            };
            if let Err(error) = Self::append_segment_frame(&data_path, &frame) {
                slog_warn!("failed to append semantic delta: {}", error);
                return false;
            }
            let Some(identity) = semantic_artifact_identity(&data_path) else {
                return false;
            };
            self.set_persistence(Some(SemanticPersistenceState {
                identity,
                base_bytes: layout.base_bytes,
                segment_count: layout.segment_count.saturating_add(1),
                segment_bytes: layout.segment_bytes.saturating_add(frame.len()),
                valid_bytes: layout.valid_bytes.saturating_add(frame.len()),
            }));
            self.set_dirty_paths(Some(BTreeSet::new()));
            crate::write_ledger::credit(
                crate::write_ledger::Domain::SemanticDelta,
                self.project_root.display().to_string(),
                frame.len() as u64,
                0,
            );
            slog_info!(
                "semantic index delta persisted: {} files, {:.1} KB, artifact_read_bytes={}",
                changed_paths.len(),
                frame.len() as f64 / 1024.0,
                layout.bytes_read
            );
            self.schedule_compaction_if_needed(&dir, &data_path, &layout, frame.len());
            return true;
        }

        match self.write_full_snapshot_at(&dir, &data_path, false) {
            Ok(bytes_written) => {
                let Some(identity) = semantic_artifact_identity(&data_path) else {
                    return false;
                };
                self.set_persistence(Some(SemanticPersistenceState {
                    identity,
                    base_bytes: bytes_written,
                    segment_count: 0,
                    segment_bytes: 0,
                    valid_bytes: bytes_written,
                }));
                self.set_dirty_paths(Some(BTreeSet::new()));
                crate::write_ledger::credit(
                    crate::write_ledger::Domain::SemanticCold,
                    self.project_root.display().to_string(),
                    bytes_written as u64,
                    0,
                );
                slog_info!(
                    "semantic index persisted: {} entries, {:.1} KB",
                    self.entries.len(),
                    bytes_written as f64 / 1024.0
                );
                true
            }
            Err(error) => {
                slog_warn!("failed to write semantic index: {}", error);
                false
            }
        }
    }

    #[doc(hidden)]
    pub fn segment_frames_for_test(
        &self,
        previous: &Self,
        sequence: u64,
    ) -> Option<(Vec<u8>, Vec<u8>)> {
        let dirty_paths = self.dirty_paths_snapshot()?;
        let structural_paths = semantic_changed_paths(previous, self);
        Some((
            self.build_segment_frame(sequence, &dirty_paths).ok()?,
            self.build_segment_frame(sequence, &structural_paths).ok()?,
        ))
    }

    #[doc(hidden)]
    pub fn extend_dirty_paths_for_test(&self, paths: impl IntoIterator<Item = PathBuf>) {
        self.extend_dirty_paths(paths);
    }

    #[doc(hidden)]
    pub fn last_append_read_bytes_for_test(&self) -> usize {
        self.last_append_read_bytes.load(Ordering::Relaxed)
    }

    #[doc(hidden)]
    pub fn append_scan_bytes_for_test(
        &self,
        storage_dir: &Path,
        project_key: &str,
    ) -> Option<usize> {
        let data_path = storage_dir
            .join("semantic")
            .join(project_key)
            .join("semantic.bin");
        let persistence = self.persistence_snapshot()?;
        let fingerprint = self
            .fingerprint
            .as_ref()
            .map(SemanticIndexFingerprint::as_string)
            .unwrap_or_default();
        Self::scan_artifact_for_append(&data_path, persistence, &fingerprint, self.dimension)
            .ok()
            .map(|layout| layout.bytes_read)
    }

    #[doc(hidden)]
    pub fn compact_to_disk_for_test(&self, storage_dir: &Path, project_key: &str) -> bool {
        let dir = storage_dir.join("semantic").join(project_key);
        let data_path = dir.join("semantic.bin");
        let Some(expected) = semantic_artifact_identity(&data_path) else {
            return false;
        };
        Self::compact_path_if_unchanged(&dir, &data_path, &self.project_root, expected)
    }

    #[doc(hidden)]
    pub fn persistence_stats_for_test(
        storage_dir: &Path,
        project_key: &str,
        project_root: &Path,
    ) -> Option<(usize, usize, usize)> {
        let data_path = storage_dir
            .join("semantic")
            .join(project_key)
            .join("semantic.bin");
        let loaded = Self::load_artifact_path(&data_path, project_root).ok()?;
        Some((
            loaded.base_bytes,
            loaded.segment_count,
            loaded.segment_bytes,
        ))
    }

    fn decode_segment_payload(
        payload: &[u8],
        expected_sequence: u64,
        current_canonical_root: &Path,
        base_fingerprint: Option<&SemanticIndexFingerprint>,
        base_dimension: usize,
    ) -> Result<(BTreeSet<PathBuf>, Self), String> {
        let mut reader = CountingReader::with_bytes_read(Cursor::new(payload), 0);
        let segment_version = read_u8_stream(&mut reader, "semantic segment is empty")?;
        if segment_version != SEMANTIC_SEGMENT_VERSION {
            return Err(format!(
                "unsupported semantic segment version: {segment_version}"
            ));
        }
        let sequence = read_u64_stream(&mut reader)?;
        if sequence != expected_sequence {
            return Err(format!(
                "semantic segment order mismatch: expected {expected_sequence}, found {sequence}"
            ));
        }
        let fingerprint_len = read_u32_stream(&mut reader)? as usize;
        if reader.bytes_read().saturating_add(fingerprint_len) > payload.len() {
            return Err("unexpected end of semantic segment fingerprint".to_string());
        }
        let mut fingerprint = vec![0_u8; fingerprint_len];
        read_exact_stream(
            &mut reader,
            &mut fingerprint,
            "unexpected end of semantic segment fingerprint",
        )?;
        let fingerprint = String::from_utf8(fingerprint)
            .map_err(|error| format!("invalid semantic segment fingerprint: {error}"))?;
        let expected_fingerprint = base_fingerprint
            .map(SemanticIndexFingerprint::as_string)
            .unwrap_or_default();
        if fingerprint != expected_fingerprint {
            return Err("semantic segment fingerprint does not match base snapshot".to_string());
        }

        let dimension = read_u32_stream(&mut reader)? as usize;
        if dimension != base_dimension {
            return Err(format!(
                "semantic segment dimension mismatch: base={base_dimension}, segment={dimension}"
            ));
        }
        let tombstone_count = read_u32_stream(&mut reader)? as usize;
        if tombstone_count > MAX_ENTRIES {
            return Err(format!(
                "too many semantic segment tombstones: {tombstone_count}"
            ));
        }
        let mut tombstones = BTreeSet::new();
        for _ in 0..tombstone_count {
            let relative = PathBuf::from(read_string_stream(&mut reader, Some(payload.len()))?);
            let path = cached_path_under_root(current_canonical_root, &relative)
                .ok_or_else(|| "semantic segment tombstone escapes project root".to_string())?;
            if !tombstones.insert(path) {
                return Err("semantic segment contains a duplicate tombstone".to_string());
            }
        }

        let delta_len = usize::try_from(read_u64_stream(&mut reader)?)
            .map_err(|_| "semantic segment delta is too large".to_string())?;
        if reader.bytes_read().saturating_add(delta_len) != payload.len() {
            return Err("semantic segment delta length does not match payload".to_string());
        }
        let mut delta_bytes = vec![0_u8; delta_len];
        read_exact_stream(
            &mut reader,
            &mut delta_bytes,
            "unexpected end of semantic segment delta",
        )?;
        let delta = Self::from_bytes(&delta_bytes, current_canonical_root)?;
        if delta.dimension != base_dimension
            || delta
                .fingerprint
                .as_ref()
                .map(SemanticIndexFingerprint::as_string)
                != base_fingerprint.map(SemanticIndexFingerprint::as_string)
        {
            return Err("semantic segment replacement snapshot identity mismatch".to_string());
        }

        let replacement_paths = delta
            .file_mtimes
            .keys()
            .chain(delta.file_sizes.keys())
            .chain(delta.file_hashes.keys())
            .cloned()
            .chain(delta.entries.iter().map(|entry| entry.chunk.file.clone()))
            .collect::<BTreeSet<_>>();
        if !replacement_paths.is_subset(&tombstones) {
            return Err(
                "semantic segment replacement contains a file without a tombstone".to_string(),
            );
        }
        Ok((tombstones, delta))
    }

    fn apply_segment_log<R: Read>(
        reader: &mut R,
        mut index: Self,
        total_len: usize,
        base_bytes: usize,
    ) -> Result<LoadedSemanticArtifact, String> {
        let mut valid_bytes = base_bytes;
        let mut segment_count = 0usize;
        let mut torn_tail = false;

        while valid_bytes < total_len {
            let remaining = total_len.saturating_sub(valid_bytes);
            if remaining < SEMANTIC_SEGMENT_FRAME_HEADER_BYTES {
                torn_tail = true;
                break;
            }
            let mut header = [0_u8; SEMANTIC_SEGMENT_FRAME_HEADER_BYTES];
            if reader.read_exact(&mut header).is_err() {
                torn_tail = true;
                break;
            }
            if &header[..SEMANTIC_SEGMENT_MAGIC.len()] != SEMANTIC_SEGMENT_MAGIC {
                torn_tail = true;
                break;
            }
            let payload_len = usize::try_from(u64::from_le_bytes(
                header[8..16]
                    .try_into()
                    .expect("semantic segment length field"),
            ))
            .map_err(|_| "semantic segment length exceeds this platform".to_string())?;
            let frame_len = SEMANTIC_SEGMENT_FRAME_HEADER_BYTES
                .checked_add(payload_len)
                .ok_or_else(|| "semantic segment frame length overflow".to_string())?;
            if frame_len > remaining {
                torn_tail = true;
                break;
            }
            let mut payload = vec![0_u8; payload_len];
            if reader.read_exact(&mut payload).is_err() {
                torn_tail = true;
                break;
            }
            let expected_checksum = &header[16..SEMANTIC_SEGMENT_FRAME_HEADER_BYTES];
            if blake3::hash(&payload).as_bytes() != expected_checksum {
                torn_tail = true;
                break;
            }

            let expected_sequence = segment_count.saturating_add(1) as u64;
            let (tombstones, delta) = Self::decode_segment_payload(
                &payload,
                expected_sequence,
                &index.project_root,
                index.fingerprint.as_ref(),
                index.dimension,
            )?;
            let tombstones = tombstones.into_iter().collect::<Vec<_>>();
            index.remove_indexed_files(&tombstones);
            index.entries.extend(delta.entries);
            index.file_mtimes.extend(delta.file_mtimes);
            index.file_sizes.extend(delta.file_sizes);
            index.file_hashes.extend(delta.file_hashes);
            index.any_missing_sizes = index
                .file_mtimes
                .keys()
                .any(|path| !index.file_sizes.contains_key(path));

            valid_bytes = valid_bytes.saturating_add(frame_len);
            segment_count = segment_count.saturating_add(1);
        }

        Ok(LoadedSemanticArtifact {
            index,
            base_bytes,
            valid_bytes,
            segment_count,
            segment_bytes: valid_bytes.saturating_sub(base_bytes),
            torn_tail,
        })
    }

    fn load_artifact_path(
        data_path: &Path,
        current_canonical_root: &Path,
    ) -> Result<LoadedSemanticArtifact, String> {
        let file = fs::File::open(data_path).map_err(|error| error.to_string())?;
        let file_len =
            usize::try_from(file.metadata().map_err(|error| error.to_string())?.len())
                .map_err(|_| "semantic artifact is too large for this platform".to_string())?;
        if file_len < HEADER_BYTES_V1 {
            return Err(format!("data too short: {file_len} bytes"));
        }
        let mut reader = BufReader::new(file);
        let mut version_buf = [0_u8; 1];
        reader
            .read_exact(&mut version_buf)
            .map_err(|error| error.to_string())?;
        let version = version_buf[0];
        if version != SEMANTIC_INDEX_VERSION_V6 && version != SEMANTIC_INDEX_VERSION_V7 {
            return Err(format!("unsupported on-disk semantic version: {version}"));
        }
        let (index, base_bytes) = Self::from_reader_after_version(
            &mut reader,
            version,
            current_canonical_root,
            Some(file_len),
            1,
        )?;
        let loaded = Self::apply_segment_log(&mut reader, index, file_len, base_bytes)?;
        loaded.index.set_dirty_paths(Some(BTreeSet::new()));
        loaded
            .index
            .set_persistence(Self::persistence_state_from_loaded(data_path, &loaded));
        Ok(loaded)
    }

    /// Read the semantic base snapshot and apply every committed delta in sequence.
    pub fn read_from_disk(
        storage_dir: &Path,
        project_key: &str,
        current_canonical_root: &Path,
        is_worktree_bridge: bool,
        expected_fingerprint: Option<&str>,
    ) -> Option<Self> {
        debug_assert!(current_canonical_root.is_absolute());
        let data_path = storage_dir
            .join("semantic")
            .join(project_key)
            .join("semantic.bin");
        let file_len = usize::try_from(data_path.metadata().ok()?.len()).ok()?;
        if file_len < HEADER_BYTES_V1 {
            slog_warn!(
                "corrupt semantic index (too small: {} bytes), removing",
                file_len
            );
            if !is_worktree_bridge {
                let _ = fs::remove_file(&data_path);
            }
            return None;
        }
        let mut version_buf = [0_u8; 1];
        fs::File::open(&data_path)
            .ok()?
            .read_exact(&mut version_buf)
            .ok()?;
        let version = version_buf[0];
        if version != SEMANTIC_INDEX_VERSION_V6 && version != SEMANTIC_INDEX_VERSION_V7 {
            slog_info!(
                "cached semantic index version {} is not compatible with {}, rebuilding without deleting the shared artifact",
                version,
                SEMANTIC_INDEX_VERSION_V7
            );
            return None;
        }

        match Self::load_artifact_path(&data_path, current_canonical_root) {
            Ok(loaded) => {
                if let Some(expected) = expected_fingerprint {
                    let matches = loaded
                        .index
                        .fingerprint()
                        .map(|fingerprint| fingerprint.matches_expected(expected))
                        .unwrap_or(false);
                    if !matches {
                        log_fingerprint_mismatch(loaded.index.fingerprint(), expected);
                        return None;
                    }
                }
                if loaded.torn_tail {
                    slog_warn!(
                        "ignoring torn semantic segment tail after {} committed bytes",
                        loaded.valid_bytes
                    );
                    if !is_worktree_bridge {
                        let truncate_result = OpenOptions::new()
                            .write(true)
                            .open(&data_path)
                            .and_then(|file| {
                                file.set_len(loaded.valid_bytes as u64)?;
                                file.sync_all()
                            });
                        if let Err(error) = truncate_result {
                            slog_warn!("failed to truncate torn semantic segment: {}", error);
                        }
                    }
                }
                slog_info!(
                    "loaded semantic index from disk: {} entries ({} delta segments)",
                    loaded.index.entries.len(),
                    loaded.segment_count
                );
                Some(loaded.index)
            }
            Err(error) => {
                slog_warn!("corrupt semantic index, rebuilding: {}", error);
                if !is_worktree_bridge {
                    let _ = fs::remove_file(&data_path);
                }
                None
            }
        }
    }

    pub(crate) fn read_from_disk_borrow_tolerant(
        storage_dir: &Path,
        project_key: &str,
        current_canonical_root: &Path,
    ) -> Option<Self> {
        let load_started = Instant::now();
        let loaded = Self::read_from_disk_borrow_tolerant_inner(
            storage_dir,
            project_key,
            current_canonical_root,
        );
        let outcome = if loaded.is_some() { "ready" } else { "denied" };
        let build_id = crate::logging::in_flight_build_id(
            crate::logging::IndexPlane::Semantic,
            current_canonical_root,
        )
        .unwrap_or_else(crate::logging::mint_index_build_id);
        crate::logging::log_index_event(
            crate::logging::IndexEvent::new(
                crate::logging::IndexEventKind::ArtifactLoaded,
                crate::logging::IndexPlane::Semantic,
                build_id,
                current_canonical_root,
                project_key,
            )
            .field("outcome", outcome)
            .field("borrowed", "true")
            .field(
                "elapsed_ms",
                load_started.elapsed().as_millis().min(u64::MAX as u128) as u64,
            ),
        );
        crate::logging::note_tool_call_wait(
            crate::run_tool_call::WaitingOn::ArtifactLoad,
            None,
            load_started.elapsed().as_millis().min(u64::MAX as u128) as u64,
        );
        loaded
    }

    fn read_from_disk_borrow_tolerant_inner(
        storage_dir: &Path,
        project_key: &str,
        current_canonical_root: &Path,
    ) -> Option<Self> {
        let data_path = storage_dir
            .join("semantic")
            .join(project_key)
            .join("semantic.bin");
        let (fingerprint, artifact_content_hash) = match borrowed_artifact_identity(&data_path) {
            Ok(identity) => identity,
            Err(error) => {
                slog_warn!(
                    "semantic shared-base identity unavailable ({}); loading a private borrowed copy",
                    error
                );
                return Self::read_from_disk(
                    storage_dir,
                    project_key,
                    current_canonical_root,
                    true,
                    None,
                );
            }
        };
        let key = SharedSemanticBaseKey {
            artifact_cache_key: project_key.to_string(),
            fingerprint,
            artifact_content_hash,
        };

        {
            let mut registry = shared_semantic_bases()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            registry.retain(|_, base| base.strong_count() > 0);
            if let Some(base) = registry.get(&key).and_then(Weak::upgrade) {
                SHARED_SEMANTIC_BASE_HITS.fetch_add(1, Ordering::Relaxed);
                return Some(Self::from_shared_base(
                    current_canonical_root.to_path_buf(),
                    base,
                ));
            }
            if registry.keys().any(|existing| {
                existing.artifact_cache_key == key.artifact_cache_key && existing != &key
            }) {
                slog_warn!(
                    "semantic shared-base fingerprint or artifact hash changed for key {}; loading a private borrowed copy",
                    project_key
                );
                return Self::read_from_disk(
                    storage_dir,
                    project_key,
                    current_canonical_root,
                    true,
                    None,
                );
            }
        }

        let private = Self::read_from_disk(
            storage_dir,
            project_key,
            current_canonical_root,
            true,
            Some(&key.fingerprint),
        )?;
        let Ok(base) = private.clone().into_shared_base() else {
            slog_warn!(
                "semantic shared-base paths could not be normalized for key {}; loading a private borrowed copy",
                project_key
            );
            return Some(private);
        };
        let base = Arc::new(base);

        let mut registry = shared_semantic_bases()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        registry.retain(|_, base| base.strong_count() > 0);
        if let Some(existing) = registry.get(&key).and_then(Weak::upgrade) {
            SHARED_SEMANTIC_BASE_HITS.fetch_add(1, Ordering::Relaxed);
            return Some(Self::from_shared_base(
                current_canonical_root.to_path_buf(),
                existing,
            ));
        }
        if registry.keys().any(|existing| {
            existing.artifact_cache_key == key.artifact_cache_key && existing != &key
        }) {
            slog_warn!(
                "semantic shared-base identity changed while loading key {}; retaining a private borrowed copy",
                project_key
            );
            return Some(private);
        }
        registry.insert(key, Arc::downgrade(&base));
        SHARED_SEMANTIC_BASE_LOADS.fetch_add(1, Ordering::Relaxed);
        Some(Self::from_shared_base(
            current_canonical_root.to_path_buf(),
            base,
        ))
    }

    /// Serialize the index to bytes for disk persistence
    pub fn to_bytes(&self) -> Vec<u8> {
        if self.shared_base.is_some() {
            let mut private = self.clone();
            private.materialize_shared_base();
            return private.to_bytes();
        }
        let mut buf = Vec::new();
        self.write_to_writer(&mut buf)
            .expect("writing semantic index to Vec cannot fail");
        buf
    }

    fn write_to_writer<W: Write>(&self, writer: &mut W) -> io::Result<usize> {
        let mut bytes_written = 0usize;
        let fingerprint = self.fingerprint.as_ref().and_then(|fingerprint| {
            let encoded = fingerprint.as_string();
            if encoded.is_empty() {
                None
            } else {
                Some(encoded)
            }
        });
        let fp_bytes_ref = fingerprint.as_deref().map(str::as_bytes).unwrap_or(&[]);
        let mut file_metadata = self
            .file_mtimes
            .iter()
            .filter_map(|(path, mtime)| {
                cache_relative_path(&self.project_root, path)
                    .map(|relative| (relative, path, mtime))
            })
            .collect::<Vec<_>>();
        file_metadata.sort_by(|left, right| left.0.cmp(&right.0));
        let mut persisted_entries = self
            .entries
            .iter()
            .filter_map(|entry| {
                cache_relative_path(&self.project_root, &entry.chunk.file)
                    .map(|relative| (relative, entry))
            })
            .collect::<Vec<_>>();
        persisted_entries.sort_by(|left, right| {
            left.0
                .cmp(&right.0)
                .then_with(|| semantic_entry_cmp(&left.1, &right.1))
        });
        let file_mtime_count = file_metadata.len();
        let entry_count = persisted_entries.len();

        // Header: version(1) + dimension(4) + entry_count(4) + fingerprint_len(4) + fingerprint
        //
        // V7 is the single write format. Layout extends V6 with per-entry
        // qualified_name metadata while preserving the embedding fingerprint:
        //   - fingerprint is always represented (absent ⇒ fingerprint_len=0,
        //     no bytes follow). Uniform format simplifies the reader.
        //   - paths are relative to project_root.
        //   - file metadata stored as secs(u64) + subsec_nanos(u32) + size(u64) + blake3(32).
        //     Preserves full APFS/ext4/NTFS precision and catches mtime ties.
        //
        // V1/V2 remain readable for backward compatibility (see from_bytes).
        // V3/V4 load as compatible formats but are rejected on disk so snippets
        // and file sizes are rebuilt once. V6 remains accepted on disk and
        // yields qualified_name=None until the next V7 write.
        let version = SEMANTIC_INDEX_VERSION_V7;
        write_counted(writer, &[version], &mut bytes_written)?;
        write_counted(
            writer,
            &(self.dimension as u32).to_le_bytes(),
            &mut bytes_written,
        )?;
        write_counted(
            writer,
            &(entry_count as u32).to_le_bytes(),
            &mut bytes_written,
        )?;
        write_counted(
            writer,
            &(fp_bytes_ref.len() as u32).to_le_bytes(),
            &mut bytes_written,
        )?;
        write_counted(writer, fp_bytes_ref, &mut bytes_written)?;

        // File mtime table: count(4) + entries
        // V3 layout per entry: path_len(4) + path + secs(8) + subsec_nanos(4)
        write_counted(
            writer,
            &(file_mtime_count as u32).to_le_bytes(),
            &mut bytes_written,
        )?;
        for (relative, path, mtime) in file_metadata {
            let relative = relative.to_string_lossy();
            let path_bytes = relative.as_bytes();
            write_counted(
                writer,
                &(path_bytes.len() as u32).to_le_bytes(),
                &mut bytes_written,
            )?;
            write_counted(writer, path_bytes, &mut bytes_written)?;
            let duration = mtime
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default();
            write_counted(
                writer,
                &duration.as_secs().to_le_bytes(),
                &mut bytes_written,
            )?;
            write_counted(
                writer,
                &duration.subsec_nanos().to_le_bytes(),
                &mut bytes_written,
            )?;
            let size = self.file_sizes.get(path).copied().unwrap_or_default();
            write_counted(writer, &size.to_le_bytes(), &mut bytes_written)?;
            let hash = self
                .file_hashes
                .get(path)
                .copied()
                .unwrap_or_else(cache_freshness::zero_hash);
            write_counted(writer, hash.as_bytes(), &mut bytes_written)?;
        }

        // Entries: each is metadata + vector. Canonical ordering lets parity
        // compare structure directly even when HashMap insertion order differs.
        for (relative, entry) in persisted_entries {
            let c = &entry.chunk;

            // File path
            let relative = relative.to_string_lossy();
            let file_bytes = relative.as_bytes();
            write_counted(
                writer,
                &(file_bytes.len() as u32).to_le_bytes(),
                &mut bytes_written,
            )?;
            write_counted(writer, file_bytes, &mut bytes_written)?;

            // Name
            let name_bytes = c.name.as_bytes();
            write_counted(
                writer,
                &(name_bytes.len() as u32).to_le_bytes(),
                &mut bytes_written,
            )?;
            write_counted(writer, name_bytes, &mut bytes_written)?;

            // Qualified name (V7 metadata; absent is encoded as length 0)
            let qualified_name_bytes = c.qualified_name.as_deref().unwrap_or_default().as_bytes();
            write_counted(
                writer,
                &(qualified_name_bytes.len() as u32).to_le_bytes(),
                &mut bytes_written,
            )?;
            write_counted(writer, qualified_name_bytes, &mut bytes_written)?;

            // Kind (1 byte)
            write_counted(writer, &[symbol_kind_to_u8(&c.kind)], &mut bytes_written)?;

            // Lines + exported
            write_counted(
                writer,
                &(c.start_line as u32).to_le_bytes(),
                &mut bytes_written,
            )?;
            write_counted(
                writer,
                &(c.end_line as u32).to_le_bytes(),
                &mut bytes_written,
            )?;
            write_counted(writer, &[c.exported as u8], &mut bytes_written)?;

            // Snippet
            let snippet_bytes = c.snippet.as_bytes();
            write_counted(
                writer,
                &(snippet_bytes.len() as u32).to_le_bytes(),
                &mut bytes_written,
            )?;
            write_counted(writer, snippet_bytes, &mut bytes_written)?;

            // Embed text
            let embed_bytes = c.embed_text.as_bytes();
            write_counted(
                writer,
                &(embed_bytes.len() as u32).to_le_bytes(),
                &mut bytes_written,
            )?;
            write_counted(writer, embed_bytes, &mut bytes_written)?;

            // Vector (f32 array)
            for &val in &entry.vector {
                write_counted(writer, &val.to_le_bytes(), &mut bytes_written)?;
            }
        }

        Ok(bytes_written)
    }

    /// Deserialize a base snapshot and any committed delta segments.
    pub fn from_bytes(data: &[u8], current_canonical_root: &Path) -> Result<Self, String> {
        debug_assert!(current_canonical_root.is_absolute());
        if data.len() < HEADER_BYTES_V1 {
            return Err("data too short".to_string());
        }

        let mut reader = Cursor::new(&data[1..]);
        let (index, base_bytes) = Self::from_reader_after_version(
            &mut reader,
            data[0],
            current_canonical_root,
            Some(data.len()),
            1,
        )?;
        Self::apply_segment_log(&mut reader, index, data.len(), base_bytes)
            .map(|loaded| loaded.index)
    }

    fn from_reader_after_version<R: Read>(
        reader: R,
        version: u8,
        current_canonical_root: &Path,
        total_len: Option<usize>,
        bytes_read: usize,
    ) -> Result<(Self, usize), String> {
        debug_assert!(current_canonical_root.is_absolute());
        let mut reader = CountingReader::with_bytes_read(reader, bytes_read);

        if version != SEMANTIC_INDEX_VERSION_V1
            && version != SEMANTIC_INDEX_VERSION_V2
            && version != SEMANTIC_INDEX_VERSION_V3
            && version != SEMANTIC_INDEX_VERSION_V4
            && version != SEMANTIC_INDEX_VERSION_V5
            && version != SEMANTIC_INDEX_VERSION_V6
            && version != SEMANTIC_INDEX_VERSION_V7
        {
            return Err(format!("unsupported version: {}", version));
        }
        // V2 and newer share the same header layout (V3/V4/V5 only differ from
        // V2 in the per-mtime entry layout): version(1) + dimension(4) +
        // entry_count(4) + fingerprint_len(4) + fingerprint bytes.
        if (version == SEMANTIC_INDEX_VERSION_V2
            || version == SEMANTIC_INDEX_VERSION_V3
            || version == SEMANTIC_INDEX_VERSION_V4
            || version == SEMANTIC_INDEX_VERSION_V5
            || version == SEMANTIC_INDEX_VERSION_V6
            || version == SEMANTIC_INDEX_VERSION_V7)
            && total_len.is_some_and(|len| len < HEADER_BYTES_V2)
        {
            return Err("data too short for semantic index v2/v3/v4/v5/v6/v7 header".to_string());
        }

        let dimension = read_u32_stream(&mut reader)? as usize;
        let entry_count = read_u32_stream(&mut reader)? as usize;
        validate_embedding_dimension(dimension)?;
        if entry_count > MAX_ENTRIES {
            return Err(format!("too many semantic index entries: {}", entry_count));
        }

        // Fingerprint handling:
        //   - V1: no fingerprint field at all.
        //   - V2: fingerprint_len + fingerprint bytes; always present (writer
        //     only emitted V2 when fingerprint was Some).
        //   - V3+: fingerprint_len always present; fingerprint_len==0 ⇒ None.
        let has_fingerprint_field = version == SEMANTIC_INDEX_VERSION_V2
            || version == SEMANTIC_INDEX_VERSION_V3
            || version == SEMANTIC_INDEX_VERSION_V4
            || version == SEMANTIC_INDEX_VERSION_V5
            || version == SEMANTIC_INDEX_VERSION_V6
            || version == SEMANTIC_INDEX_VERSION_V7;
        let fingerprint = if has_fingerprint_field {
            let fingerprint_len = read_u32_stream(&mut reader)? as usize;
            if total_len
                .is_some_and(|len| reader.bytes_read().saturating_add(fingerprint_len) > len)
            {
                return Err("unexpected end of data reading fingerprint".to_string());
            }
            if fingerprint_len == 0 {
                None
            } else {
                let mut raw = vec![0u8; fingerprint_len];
                read_exact_stream(
                    &mut reader,
                    &mut raw,
                    "unexpected end of data reading fingerprint",
                )?;
                let raw = String::from_utf8_lossy(&raw).to_string();
                Some(
                    serde_json::from_str::<SemanticIndexFingerprint>(&raw)
                        .map_err(|error| format!("invalid semantic fingerprint: {error}"))?,
                )
            }
        } else {
            None
        };

        // File mtimes
        let mtime_count = read_u32_stream(&mut reader)? as usize;
        if mtime_count > MAX_ENTRIES {
            return Err(format!("too many semantic file mtimes: {}", mtime_count));
        }

        let vector_bytes = entry_count
            .checked_mul(dimension)
            .and_then(|count| count.checked_mul(F32_BYTES))
            .ok_or_else(|| "semantic vector allocation overflow".to_string())?;
        if total_len.is_some_and(|len| vector_bytes > len.saturating_sub(reader.bytes_read())) {
            return Err("semantic index vectors exceed available data".to_string());
        }

        let mut file_mtimes = HashMap::with_capacity(mtime_count);
        let mut file_sizes = HashMap::with_capacity(mtime_count);
        let mut file_hashes = HashMap::with_capacity(mtime_count);
        for _ in 0..mtime_count {
            let path = read_string_stream(&mut reader, total_len)?;
            let secs = read_u64_stream(&mut reader)?;
            // V3+ persists subsec_nanos alongside secs so staleness checks
            // survive restart round-trips. V1/V2 load with 0 nanos, which
            // causes one rebuild on upgrade (they never matched live APFS
            // mtimes anyway — the bug v0.15.2 fixes). After that rebuild,
            // the cache is persisted as V3 and stabilises.
            let nanos = if version == SEMANTIC_INDEX_VERSION_V3
                || version == SEMANTIC_INDEX_VERSION_V4
                || version == SEMANTIC_INDEX_VERSION_V5
                || version == SEMANTIC_INDEX_VERSION_V6
                || version == SEMANTIC_INDEX_VERSION_V7
            {
                read_u32_stream(&mut reader)?
            } else {
                0
            };
            let size = if version == SEMANTIC_INDEX_VERSION_V5
                || version == SEMANTIC_INDEX_VERSION_V6
                || version == SEMANTIC_INDEX_VERSION_V7
            {
                read_u64_stream(&mut reader)?
            } else {
                0
            };
            let content_hash =
                if version == SEMANTIC_INDEX_VERSION_V6 || version == SEMANTIC_INDEX_VERSION_V7 {
                    let mut hash_bytes = [0u8; 32];
                    read_exact_stream(
                        &mut reader,
                        &mut hash_bytes,
                        "unexpected end of data reading content hash",
                    )?;
                    blake3::Hash::from_bytes(hash_bytes)
                } else {
                    cache_freshness::zero_hash()
                };
            // Hardening against corrupt / maliciously crafted cache files
            // (v0.15.2). `Duration::new(secs, nanos)` can panic when the
            // nanosecond carry overflows the second counter, and
            // `SystemTime + Duration` can panic on carry past the platform's
            // upper bound. Explicit validation keeps a corrupted semantic.bin
            // from taking down the whole aft process.
            if nanos >= 1_000_000_000 {
                return Err(format!(
                    "invalid semantic mtime: nanos {} >= 1_000_000_000",
                    nanos
                ));
            }
            let duration = std::time::Duration::new(secs, nanos);
            let mtime = SystemTime::UNIX_EPOCH
                .checked_add(duration)
                .ok_or_else(|| {
                    format!(
                        "invalid semantic mtime: secs={} nanos={} overflows SystemTime",
                        secs, nanos
                    )
                })?;
            let path = if version == SEMANTIC_INDEX_VERSION_V6
                || version == SEMANTIC_INDEX_VERSION_V7
            {
                cached_path_under_root(current_canonical_root, &PathBuf::from(path))
                    .ok_or_else(|| "cached semantic mtime path escapes project root".to_string())?
            } else {
                PathBuf::from(path)
            };
            file_mtimes.insert(path.clone(), mtime);
            file_sizes.insert(path.clone(), size);
            file_hashes.insert(path, content_hash);
        }

        // Entries
        let mut entries = Vec::with_capacity(entry_count);
        for _ in 0..entry_count {
            let raw_file = PathBuf::from(read_string_stream(&mut reader, total_len)?);
            let file = if version == SEMANTIC_INDEX_VERSION_V6
                || version == SEMANTIC_INDEX_VERSION_V7
            {
                cached_path_under_root(current_canonical_root, &raw_file)
                    .ok_or_else(|| "cached semantic entry path escapes project root".to_string())?
            } else {
                raw_file
            };
            let name = read_string_stream(&mut reader, total_len)?;
            let qualified_name = if version == SEMANTIC_INDEX_VERSION_V7 {
                let qualified_name = read_string_stream(&mut reader, total_len)?;
                if qualified_name.is_empty() {
                    None
                } else {
                    Some(qualified_name)
                }
            } else {
                None
            };

            let kind = u8_to_symbol_kind(read_u8_stream(&mut reader, "unexpected end of data")?);

            let start_line = read_u32_stream(&mut reader)?;
            let end_line = read_u32_stream(&mut reader)?;

            let exported = read_u8_stream(&mut reader, "unexpected end of data")? != 0;

            let snippet = read_string_stream(&mut reader, total_len)?;
            let embed_text = read_string_stream(&mut reader, total_len)?;

            // Vector
            let vec_bytes = dimension
                .checked_mul(F32_BYTES)
                .ok_or_else(|| "semantic vector allocation overflow".to_string())?;
            if total_len.is_some_and(|len| reader.bytes_read().saturating_add(vec_bytes) > len) {
                return Err("unexpected end of data reading vector".to_string());
            }
            let mut vector = Vec::with_capacity(dimension);
            for _ in 0..dimension {
                let mut bytes = [0u8; F32_BYTES];
                read_exact_stream(
                    &mut reader,
                    &mut bytes,
                    "unexpected end of data reading vector",
                )?;
                vector.push(f32::from_le_bytes(bytes));
            }

            entries.push(EmbeddingEntry::new(
                SemanticChunk {
                    file,
                    name,
                    qualified_name,
                    kind,
                    start_line,
                    end_line,
                    exported,
                    embed_text,
                    snippet,
                },
                vector,
            ));
        }

        if entries.len() != entry_count {
            return Err(format!(
                "semantic cache entry count drift: header={} decoded={}",
                entry_count,
                entries.len()
            ));
        }
        for entry in &entries {
            if !file_mtimes.contains_key(&entry.chunk.file) {
                return Err(format!(
                    "semantic cache metadata missing for entry file {}",
                    entry.chunk.file.display()
                ));
            }
        }

        let any_missing_sizes = file_mtimes
            .keys()
            .any(|path| !file_sizes.contains_key(path));
        let bytes_read = reader.bytes_read();
        Ok((
            Self {
                entries,
                file_mtimes,
                file_sizes,
                any_missing_sizes,
                file_hashes,
                dimension,
                fingerprint,
                project_root: current_canonical_root.to_path_buf(),
                deferred_files: HashSet::new(),
                shared_base: None,
                dirty_paths: Arc::new(Mutex::new(None)),
                persistence: Arc::new(Mutex::new(None)),
                last_append_read_bytes: Arc::new(AtomicUsize::new(0)),
                skipped_rows: 0,
                #[cfg(test)]
                removal_retain_passes: 0,
            },
            bytes_read,
        ))
    }
}

fn write_counted<W: Write>(
    writer: &mut W,
    bytes: &[u8],
    bytes_written: &mut usize,
) -> io::Result<()> {
    writer.write_all(bytes)?;
    *bytes_written = bytes_written.saturating_add(bytes.len());
    Ok(())
}

struct CountingReader<R> {
    inner: R,
    bytes_read: usize,
}

impl<R> CountingReader<R> {
    fn with_bytes_read(inner: R, bytes_read: usize) -> Self {
        Self { inner, bytes_read }
    }

    fn bytes_read(&self) -> usize {
        self.bytes_read
    }
}

impl<R: Read> Read for CountingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buf)?;
        self.bytes_read = self.bytes_read.saturating_add(read);
        Ok(read)
    }
}

fn read_exact_stream<R: Read>(
    reader: &mut CountingReader<R>,
    buf: &mut [u8],
    eof_message: &'static str,
) -> Result<(), String> {
    reader.read_exact(buf).map_err(|error| {
        if error.kind() == io::ErrorKind::UnexpectedEof {
            eof_message.to_string()
        } else {
            format!("{eof_message}: {error}")
        }
    })
}

fn read_u8_stream<R: Read>(
    reader: &mut CountingReader<R>,
    eof_message: &'static str,
) -> Result<u8, String> {
    let mut bytes = [0u8; 1];
    read_exact_stream(reader, &mut bytes, eof_message)?;
    Ok(bytes[0])
}

fn read_u32_stream<R: Read>(reader: &mut CountingReader<R>) -> Result<u32, String> {
    let mut bytes = [0u8; 4];
    read_exact_stream(reader, &mut bytes, "unexpected end of data reading u32")?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_u64_stream<R: Read>(reader: &mut CountingReader<R>) -> Result<u64, String> {
    let mut bytes = [0u8; 8];
    read_exact_stream(reader, &mut bytes, "unexpected end of data reading u64")?;
    Ok(u64::from_le_bytes(bytes))
}

fn read_string_stream<R: Read>(
    reader: &mut CountingReader<R>,
    total_len: Option<usize>,
) -> Result<String, String> {
    let len = read_u32_stream(reader)? as usize;
    if total_len.is_some_and(|total_len| reader.bytes_read().saturating_add(len) > total_len) {
        return Err("unexpected end of data reading string".to_string());
    }
    let mut bytes = vec![0u8; len];
    read_exact_stream(reader, &mut bytes, "unexpected end of data reading string")?;
    Ok(String::from_utf8_lossy(&bytes).to_string())
}

struct SourceLineCache<'a> {
    lines: Vec<&'a str>,
    line_starts: Vec<usize>,
}

impl<'a> SourceLineCache<'a> {
    fn new(source: &'a str) -> Self {
        let lines: Vec<&'a str> = source.lines().collect();
        let mut line_starts = Vec::with_capacity(lines.len());
        let bytes = source.as_bytes();
        let mut offset = 0usize;
        for line in &lines {
            line_starts.push(offset);
            offset += line.len();
            if bytes.get(offset) == Some(&b'\r') && bytes.get(offset + 1) == Some(&b'\n') {
                offset += 2;
            } else if bytes.get(offset) == Some(&b'\n') {
                offset += 1;
            }
        }
        Self { lines, line_starts }
    }

    fn len(&self) -> usize {
        debug_assert_eq!(self.lines.len(), self.line_starts.len());
        self.line_starts.len()
    }
}

/// Build enriched embedding text from a symbol with cAST-style context.
fn build_embed_text_with_lines_and_caps(
    symbol: &Symbol,
    line_cache: &SourceLineCache<'_>,
    file: &Path,
    project_root: &Path,
    caps: EmbedTextCaps,
) -> String {
    let relative = file
        .strip_prefix(project_root)
        .unwrap_or(file)
        .to_string_lossy();

    let kind_label = match &symbol.kind {
        SymbolKind::Function => "function",
        SymbolKind::Kernel => "kernel",
        SymbolKind::Class => "class",
        SymbolKind::Method => "method",
        SymbolKind::Struct => "struct",
        SymbolKind::Interface => "interface",
        SymbolKind::Enum => "enum",
        SymbolKind::TypeAlias => "type",
        SymbolKind::Variable => "variable",
        SymbolKind::Heading => "heading",
        SymbolKind::FileSummary => "file-summary",
    };

    // Build: "file:relative/path kind:function name:validateAuth signature:fn validateAuth(token: &str) -> bool"
    let name = &symbol.name;
    let mut text = format!(
        "name:{name} file:{} kind:{} name:{name}",
        relative, kind_label
    );

    if let Some(sig) = &symbol.signature {
        // Cap the signature: structured parsers (e.g. YAML/Kubernetes) pack
        // entire inline scripts (CronJob/Job `command:` bodies, multi-KB) into
        // the signature. Appending it unbounded produces a single embed_text
        // that overflows the embedding backend's physical batch (e.g. a
        // llama.cpp server's 512-token cap), aborting the whole index build
        // and silently degrading every search to lexical. 400 chars keeps the
        // identifying head of the signature without blowing the budget.
        text.push_str(&format!(
            " signature:{}",
            truncate_chars(sig, caps.signature_chars)
        ));
    }

    // Add the leading symbol body within the resolved backend budget.
    let start = (symbol.range.start_line as usize).min(line_cache.len());
    // range.end_line is inclusive 0-based; +1 makes it an exclusive slice bound.
    let end = (symbol.range.end_line as usize + 1).min(line_cache.len());
    if start < end {
        let body: String = line_cache.lines[start..end]
            .iter()
            .take(caps.body_lines)
            .copied()
            .collect::<Vec<&str>>()
            .join("\n");
        let snippet = if body.len() > caps.body_chars {
            format!("{}...", &body[..body.floor_char_boundary(caps.body_chars)])
        } else {
            body
        };
        text.push_str(&format!(" body:{}", snippet));
    }

    // Final defense-in-depth clamp: no single embed_text may exceed the
    // resolved backend budget regardless of which field grew.
    truncate_chars(&text, caps.total_chars)
}

#[cfg(test)]
fn build_embed_text(symbol: &Symbol, source: &str, file: &Path, project_root: &Path) -> String {
    let line_cache = SourceLineCache::new(source);
    build_embed_text_with_lines_and_caps(
        symbol,
        &line_cache,
        file,
        project_root,
        EmbedTextCaps::default(),
    )
}

/// Legacy whole-row character cap retained when no remote token budget is set.
const MAX_EMBED_TEXT_CHARS: usize = 1600;
const DEFAULT_SIGNATURE_CHARS: usize = 400;
const DEFAULT_BODY_LINES: usize = 15;
const DEFAULT_BODY_CHARS: usize = 300;
/// Maximum `name/file/kind/name` header measured across the six September 2026
/// semantic-census corpora. Reserving this many characters keeps the configured
/// token budget an upper bound even for the longest observed header.
pub const MAX_EMBED_TEXT_HEADER_CHARS: usize = 457;
const CHARS_PER_TOKEN_NUMERATOR: usize = 7;
const CHARS_PER_TOKEN_DENOMINATOR: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbedTextCaps {
    pub signature_chars: usize,
    pub body_lines: usize,
    pub body_chars: usize,
    pub total_chars: usize,
}

impl EmbedTextCaps {
    pub fn from_config(config: &SemanticBackendConfig) -> Self {
        let defaults = Self::default();
        if config.backend == SemanticBackend::Fastembed {
            return defaults;
        }
        let Some(max_input_tokens) = config.max_input_tokens else {
            return defaults;
        };

        let total_chars = max_input_tokens.saturating_mul(CHARS_PER_TOKEN_NUMERATOR)
            / CHARS_PER_TOKEN_DENOMINATOR;
        let body_chars = total_chars.saturating_sub(
            defaults
                .signature_chars
                .saturating_add(MAX_EMBED_TEXT_HEADER_CHARS),
        );
        Self {
            signature_chars: defaults.signature_chars,
            body_lines: usize::MAX,
            body_chars,
            total_chars,
        }
    }
}

impl Default for EmbedTextCaps {
    fn default() -> Self {
        Self {
            signature_chars: DEFAULT_SIGNATURE_CHARS,
            body_lines: DEFAULT_BODY_LINES,
            body_chars: DEFAULT_BODY_CHARS,
            total_chars: MAX_EMBED_TEXT_CHARS,
        }
    }
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

fn first_leading_doc_comment(line_cache: &SourceLineCache<'_>) -> String {
    let Some((start, first)) = line_cache
        .lines
        .iter()
        .enumerate()
        .find(|(_, line)| !line.trim().is_empty())
    else {
        return String::new();
    };

    let trimmed = first.trim_start();
    if trimmed.starts_with("/**") {
        let mut comment = Vec::new();
        for line in line_cache.lines.iter().skip(start) {
            comment.push(*line);
            if line.contains("*/") {
                break;
            }
        }
        return truncate_chars(&comment.join("\n"), 200);
    }

    if trimmed.starts_with("///") || trimmed.starts_with("//!") {
        let comment = line_cache
            .lines
            .iter()
            .skip(start)
            .take_while(|line| {
                let trimmed = line.trim_start();
                trimmed.starts_with("///") || trimmed.starts_with("//!")
            })
            .copied()
            .collect::<Vec<_>>()
            .join("\n");
        return truncate_chars(&comment, 200);
    }

    String::new()
}

pub fn build_file_summary_chunk(
    file: &Path,
    project_root: &Path,
    source: &str,
    top_exports: &[&str],
    top_export_signatures: &[Option<&str>],
) -> SemanticChunk {
    let line_cache = SourceLineCache::new(source);
    build_file_summary_chunk_with_lines(
        file,
        project_root,
        &line_cache,
        top_exports,
        top_export_signatures,
    )
}

fn build_file_summary_chunk_with_lines(
    file: &Path,
    project_root: &Path,
    line_cache: &SourceLineCache<'_>,
    top_exports: &[&str],
    top_export_signatures: &[Option<&str>],
) -> SemanticChunk {
    build_file_summary_chunk_with_lines_and_caps(
        file,
        project_root,
        line_cache,
        top_exports,
        top_export_signatures,
        EmbedTextCaps::default(),
    )
}

fn build_file_summary_chunk_with_lines_and_caps(
    file: &Path,
    project_root: &Path,
    line_cache: &SourceLineCache<'_>,
    top_exports: &[&str],
    top_export_signatures: &[Option<&str>],
    caps: EmbedTextCaps,
) -> SemanticChunk {
    let relative = file.strip_prefix(project_root).unwrap_or(file);
    let rel_path = relative.to_string_lossy();
    let parent_dir = relative
        .parent()
        .map(|parent| parent.to_string_lossy().to_string())
        .unwrap_or_default();
    let name = file
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_string())
        .unwrap_or_default();
    let doc = first_leading_doc_comment(line_cache);
    let exports = top_exports
        .iter()
        .take(5)
        .copied()
        .collect::<Vec<_>>()
        .join(",");
    let snippet = if doc.is_empty() {
        top_export_signatures
            .first()
            .and_then(|signature| signature.as_deref())
            .map(|signature| truncate_chars(signature, 200))
            .unwrap_or_default()
    } else {
        doc.clone()
    };

    SemanticChunk {
        file: file.to_path_buf(),
        name,
        qualified_name: None,
        kind: SymbolKind::FileSummary,
        start_line: 0,
        end_line: 0,
        exported: false,
        embed_text: truncate_chars(
            &format!(
                "file:{rel_path} kind:file-summary name:{} parent:{parent_dir} doc:{doc} exports:{exports}",
                file.file_stem()
                    .map(|stem| stem.to_string_lossy().to_string())
                    .unwrap_or_default()
            ),
            caps.total_chars,
        ),
        snippet,
    }
}

pub fn is_semantic_indexed_extension(path: &Path) -> bool {
    if path.file_name().and_then(|name| name.to_str()) == Some("Jenkinsfile") {
        return true;
    }

    matches!(
        path.extension().and_then(|extension| extension.to_str()),
        Some(
            "ts" | "tsx"
                | "js"
                | "jsx"
                | "py"
                | "rs"
                | "go"
                | "c"
                | "h"
                | "cc"
                | "cpp"
                | "cxx"
                | "hpp"
                | "hh"
                | "cu"
                | "cuh"
                | "metal"
                | "zig"
                | "cs"
                | "sh"
                | "bash"
                | "zsh"
                | "inc"
                | "php"
                | "sol"
                | "scss"
                | "vue"
                | "yaml"
                | "yml"
                | "pas"
                | "pp"
                | "dpr"
                | "dpk"
                | "lpr"
                | "java"
                | "kt"
                | "kts"
                | "rb"
                | "swift"
                | "scala"
                | "sc"
                | "lua"
                | "pl"
                | "pm"
                | "t"
                | "r"
                | "R"
                | "groovy"
                | "gvy"
                | "gy"
                | "gsh"
                | "gradle"
                | "m"
                | "mm"
                | "toml",
        )
    )
}

fn canonicalize_existing_or_deleted_path(path: &Path) -> PathBuf {
    if let Ok(canonical) = fs::canonicalize(path) {
        return canonical;
    }

    let Some(parent) = path.parent() else {
        return path.to_path_buf();
    };
    let Some(file_name) = path.file_name() else {
        return path.to_path_buf();
    };

    fs::canonicalize(parent)
        .map(|canonical_parent| canonical_parent.join(file_name))
        .unwrap_or_else(|_| path.to_path_buf())
}

/// Files larger than this are skipped for semantic chunking. The read +
/// tree-sitter parse is transiently O(file size) (tree-sitter can use several×
/// the source bytes), and `par_iter` collection parses many files at once, so an
/// unbounded read here is an OOM vector on a repo with a few multi-MB generated/
/// vendored/minified files. A file this large yields almost no useful embedding
/// anyway (each chunk's embed_text is bounded by its resolved backend caps), so we
/// track it (0 chunks) instead of reading it — freshness then skips it on later
/// refreshes. 4 MiB keeps essentially all hand-written source while capping the
/// pathological tail.
const MAX_SEMANTIC_FILE_BYTES: u64 = 4 * 1024 * 1024;

fn collect_semantic_file(
    project_root: &Path,
    file: &Path,
    embed_text_caps: EmbedTextCaps,
    phases: &mut SemanticCollectPhaseTimings,
) -> Result<(IndexedFileMetadata, Vec<SemanticChunk>), String> {
    let read_hash_started = Instant::now();
    let read_result = (|| {
        let metadata = fs::metadata(file).map_err(|error| error.to_string())?;
        if !metadata.is_file() {
            return Err("not a regular file".to_string());
        }
        let mtime = metadata.modified().map_err(|error| error.to_string())?;
        let size = metadata.len();

        if !is_semantic_indexed_extension(file) {
            return Err("unsupported file extension".to_string());
        }
        let lang = detect_language(file).ok_or_else(|| "unsupported file extension".to_string())?;

        let mut indexed_metadata = IndexedFileMetadata {
            mtime,
            size,
            content_hash: cache_freshness::zero_hash(),
        };

        // OOM backstop: skip oversized files before the read + parse (tracked with
        // zero chunks by the caller, so freshness won't re-read them every refresh).
        if size > MAX_SEMANTIC_FILE_BYTES {
            return Ok((indexed_metadata, lang, None));
        }

        let source = fs::read_to_string(file).map_err(|error| error.to_string())?;
        indexed_metadata.content_hash = if size <= cache_freshness::CONTENT_HASH_SIZE_CAP {
            cache_freshness::hash_bytes(source.as_bytes())
        } else {
            cache_freshness::zero_hash()
        };
        Ok((indexed_metadata, lang, Some(source)))
    })();
    phases.read_hash += read_hash_started.elapsed();
    let (indexed_metadata, lang, source) = read_result?;
    let Some(source) = source else {
        return Ok((indexed_metadata, Vec::new()));
    };

    let chunks = collect_file_chunks_from_source_timed(
        project_root,
        file,
        lang,
        &source,
        embed_text_caps,
        phases,
    )?;
    Ok((indexed_metadata, chunks))
}

#[cfg(feature = "semantic-chunk-census")]
#[doc(hidden)]
pub fn collect_file_chunks_for_census(
    project_root: &Path,
    file: &Path,
    census_caps: EmbedTextCaps,
) -> Result<(Vec<SemanticChunk>, Vec<SemanticChunk>), String> {
    if !is_semantic_indexed_extension(file) {
        return Err("unsupported file extension".to_string());
    }
    let lang = detect_language(file).ok_or_else(|| "unsupported file extension".to_string())?;
    if fs::metadata(file).is_ok_and(|metadata| metadata.len() > MAX_SEMANTIC_FILE_BYTES) {
        return Ok((Vec::new(), Vec::new()));
    }

    let source = fs::read_to_string(file).map_err(|error| error.to_string())?;
    let tree =
        parse_source_with_cached_parser(file, &source, lang).map_err(|error| error.to_string())?;
    let symbols =
        extract_symbols_from_tree(&source, &tree, lang).map_err(|error| error.to_string())?;
    let today = symbols_to_chunks(file, &symbols, &source, project_root);
    let census = symbols_to_chunks_with_caps(file, &symbols, &source, project_root, census_caps);
    Ok((today, census))
}

#[cfg(test)]
fn collect_file_chunks(project_root: &Path, file: &Path) -> Result<Vec<SemanticChunk>, String> {
    if !is_semantic_indexed_extension(file) {
        return Err("unsupported file extension".to_string());
    }
    let lang = detect_language(file).ok_or_else(|| "unsupported file extension".to_string())?;
    // OOM backstop: skip oversized files before the read + parse (tracked with
    // zero chunks by the caller, so freshness won't re-read them every refresh).
    if fs::metadata(file).is_ok_and(|m| m.len() > MAX_SEMANTIC_FILE_BYTES) {
        return Ok(Vec::new());
    }
    let source = fs::read_to_string(file).map_err(|error| error.to_string())?;
    collect_file_chunks_from_source(project_root, file, lang, &source)
}

#[cfg(test)]
fn collect_file_chunks_from_source(
    project_root: &Path,
    file: &Path,
    lang: crate::parser::LangId,
    source: &str,
) -> Result<Vec<SemanticChunk>, String> {
    collect_file_chunks_from_source_timed(
        project_root,
        file,
        lang,
        source,
        EmbedTextCaps::default(),
        &mut SemanticCollectPhaseTimings::default(),
    )
}

fn collect_file_chunks_from_source_timed(
    project_root: &Path,
    file: &Path,
    lang: crate::parser::LangId,
    source: &str,
    embed_text_caps: EmbedTextCaps,
    phases: &mut SemanticCollectPhaseTimings,
) -> Result<Vec<SemanticChunk>, String> {
    let parse_started = Instant::now();
    let tree_result =
        parse_source_with_cached_parser(file, source, lang).map_err(|error| error.to_string());
    phases.parse += parse_started.elapsed();
    let tree = tree_result?;

    let extract_started = Instant::now();
    let symbols_result =
        extract_symbols_from_tree(source, &tree, lang).map_err(|error| error.to_string());
    phases.extract += extract_started.elapsed();
    let symbols = symbols_result?;

    let build_started = Instant::now();
    let chunks = symbols_to_chunks_with_caps(file, &symbols, source, project_root, embed_text_caps);
    phases.build += build_started.elapsed();
    Ok(chunks)
}

/// Build a display snippet from a symbol's source
fn build_snippet_with_lines(symbol: &Symbol, line_cache: &SourceLineCache<'_>) -> String {
    let start = (symbol.range.start_line as usize).min(line_cache.len());
    // range.end_line is inclusive 0-based; +1 makes it an exclusive slice bound.
    let end = (symbol.range.end_line as usize + 1).min(line_cache.len());
    if start < end {
        let snippet_lines: Vec<&str> = line_cache.lines[start..end]
            .iter()
            .take(5)
            .copied()
            .collect();
        let mut snippet = snippet_lines.join("\n");
        if end - start > 5 {
            snippet.push_str("\n  ...");
        }
        if snippet.len() > 300 {
            snippet = format!("{}...", &snippet[..snippet.floor_char_boundary(300)]);
        }
        snippet
    } else {
        String::new()
    }
}

#[cfg(test)]
fn build_snippet(symbol: &Symbol, source: &str) -> String {
    let line_cache = SourceLineCache::new(source);
    build_snippet_with_lines(symbol, &line_cache)
}

fn qualified_name_for_symbol(symbol: &Symbol) -> Option<String> {
    let mut parts = symbol
        .scope_chain
        .iter()
        .filter(|part| !part.is_empty())
        .cloned()
        .collect::<Vec<_>>();
    if !symbol.name.is_empty() {
        parts.push(symbol.name.clone());
    }
    (!parts.is_empty()).then(|| parts.join("."))
}

/// Convert symbols to semantic chunks with enriched context
#[cfg(any(test, feature = "semantic-chunk-census"))]
fn symbols_to_chunks(
    file: &Path,
    symbols: &[Symbol],
    source: &str,
    project_root: &Path,
) -> Vec<SemanticChunk> {
    symbols_to_chunks_with_caps(
        file,
        symbols,
        source,
        project_root,
        EmbedTextCaps::default(),
    )
}

fn symbols_to_chunks_with_caps(
    file: &Path,
    symbols: &[Symbol],
    source: &str,
    project_root: &Path,
    caps: EmbedTextCaps,
) -> Vec<SemanticChunk> {
    let line_cache = SourceLineCache::new(source);
    let mut chunks = Vec::new();
    let top_exports_with_signatures = symbols
        .iter()
        .filter(|symbol| {
            symbol.exported
                && symbol.parent.is_none()
                && !matches!(symbol.kind, SymbolKind::Heading)
        })
        .map(|symbol| (symbol.name.as_str(), symbol.signature.as_deref()))
        .collect::<Vec<_>>();

    let has_only_headings = !symbols.is_empty()
        && symbols
            .iter()
            .all(|symbol| matches!(symbol.kind, SymbolKind::Heading));
    if top_exports_with_signatures.len() <= 2 && !has_only_headings {
        let top_exports = top_exports_with_signatures
            .iter()
            .map(|(name, _)| *name)
            .collect::<Vec<_>>();
        let top_export_signatures = top_exports_with_signatures
            .iter()
            .map(|(_, signature)| *signature)
            .collect::<Vec<_>>();
        chunks.push(build_file_summary_chunk_with_lines(
            file,
            project_root,
            &line_cache,
            &top_exports,
            &top_export_signatures,
        ));
    }

    for symbol in symbols {
        // Skip Markdown / HTML heading chunks: empirically they dominate result
        // lists even for code-shaped queries because heading prose embeds well.
        // Agents querying for code lose the actual matches under doc noise.
        // README/docs queries are still served by grep on the same files.
        if matches!(symbol.kind, SymbolKind::Heading) {
            continue;
        }

        // Skip very small symbols (single-line variables, etc.)
        let line_count = symbol
            .range
            .end_line
            .saturating_sub(symbol.range.start_line)
            + 1;
        if line_count < 2 && !matches!(symbol.kind, SymbolKind::Variable) {
            continue;
        }

        let embed_text =
            build_embed_text_with_lines_and_caps(symbol, &line_cache, file, project_root, caps);
        let snippet = build_snippet_with_lines(symbol, &line_cache);

        chunks.push(SemanticChunk {
            file: file.to_path_buf(),
            name: symbol.name.clone(),
            qualified_name: qualified_name_for_symbol(symbol),
            kind: symbol.kind.clone(),
            start_line: symbol.range.start_line,
            end_line: symbol.range.end_line,
            exported: symbol.exported,
            embed_text,
            snippet,
        });

        // Note: Nested symbols are handled separately by the outline system
        // Each symbol is indexed individually
    }

    chunks
}

fn semantic_score_order(a: &(f32, usize), b: &(f32, usize)) -> std::cmp::Ordering {
    b.0.partial_cmp(&a.0)
        .unwrap_or(std::cmp::Ordering::Equal)
        .then_with(|| a.1.cmp(&b.1))
}

/// Compute an embedding's L2 norm for its in-memory search cache.
fn vector_norm(vector: &[f32]) -> f32 {
    vector.iter().map(|value| value * value).sum::<f32>().sqrt()
}

fn dot_product(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(a, b)| a * b).sum::<f32>()
}

/// Cosine similarity reference retained for focused unit tests.
#[cfg(test)]
fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }

    let mut dot = 0.0f32;
    let mut norm_a = 0.0f32;
    let mut norm_b = 0.0f32;

    for i in 0..a.len() {
        dot += a[i] * b[i];
        norm_a += a[i] * a[i];
        norm_b += b[i] * b[i];
    }

    let denom = norm_a.sqrt() * norm_b.sqrt();
    if denom == 0.0 {
        0.0
    } else {
        dot / denom
    }
}

// Serialization helpers
fn symbol_kind_to_u8(kind: &SymbolKind) -> u8 {
    match kind {
        SymbolKind::Function => 0,
        SymbolKind::Class => 1,
        SymbolKind::Method => 2,
        SymbolKind::Struct => 3,
        SymbolKind::Interface => 4,
        SymbolKind::Enum => 5,
        SymbolKind::TypeAlias => 6,
        SymbolKind::Variable => 7,
        SymbolKind::Heading => 8,
        SymbolKind::FileSummary => 9,
        SymbolKind::Kernel => 10,
    }
}

fn u8_to_symbol_kind(v: u8) -> SymbolKind {
    match v {
        0 => SymbolKind::Function,
        1 => SymbolKind::Class,
        2 => SymbolKind::Method,
        3 => SymbolKind::Struct,
        4 => SymbolKind::Interface,
        5 => SymbolKind::Enum,
        6 => SymbolKind::TypeAlias,
        7 => SymbolKind::Variable,
        8 => SymbolKind::Heading,
        9 => SymbolKind::FileSummary,
        10 => SymbolKind::Kernel,
        _ => SymbolKind::Heading,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{SemanticBackend, SemanticBackendConfig};
    use crate::parser::FileParser;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::process::Command;
    use std::sync::atomic::{AtomicBool, AtomicU64};
    use std::thread;
    use tempfile::NamedTempFile;

    // Only the unix-gated baseline test consumes these (see its comment for
    // why Windows cannot reproduce the hash); keep Windows -D warnings clean.
    #[cfg(unix)]
    const RUST_QUERY_BASELINE_OUTPUT_HASH: &str =
        "36315439db74ed8e186076f79ed261079b2b13a4443ed4272861a2518c78d98b";

    struct CountingLocalProvider {
        calls: Arc<AtomicUsize>,
        threads: Arc<Mutex<Vec<std::thread::ThreadId>>>,
    }

    impl LocalEmbeddingProvider for CountingLocalProvider {
        fn embed(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.threads
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(std::thread::current().id());
            Ok(vec![vec![0.25, 0.5, 0.75]; texts.len()])
        }
    }

    #[test]
    fn local_build_embeddings_stay_on_the_build_caller_and_run_once() {
        let calls = Arc::new(AtomicUsize::new(0));
        let threads = Arc::new(Mutex::new(Vec::new()));
        let mut model = SemanticEmbeddingModel::from_local_provider_for_test(
            Box::new(CountingLocalProvider {
                calls: Arc::clone(&calls),
                threads: Arc::clone(&threads),
            }),
            PathBuf::from("/build-counting-test"),
        );
        let caller = std::thread::current().id();

        let vectors = model
            .embed(vec![
                "first build row".to_string(),
                "second build row".to_string(),
            ])
            .expect("build embedding");

        assert_eq!(vectors.len(), 2);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            threads
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_slice(),
            &[caller]
        );
    }

    #[cfg(unix)]
    fn rust_fixture_semantic_output_fingerprint(project_root: &Path) -> (usize, usize, String) {
        let fixture_root = project_root.join("tests/fixtures");
        // Re-materialize the fixtures with LF bytes before collecting: Windows
        // checkouts (core.autocrlf) hand collect_chunks CRLF sources, and the
        // extra byte per line shifts snippet/embed-text cap boundaries — so
        // post-hoc \r stripping cannot reproduce the LF-computed baseline.
        let lf_root = tempfile::tempdir().expect("lf fixture root");
        let fixture_files = [
            "imports_rs.rs",
            "member_rs.rs",
            "sample.rs",
            "structure_rs.rs",
        ]
        .map(|name| {
            let source = std::fs::read_to_string(fixture_root.join(name))
                .expect("read fixture")
                .replace("\r\n", "\n");
            // Preserve the tests/fixtures/<name> layout: chunk identity fields
            // (relative path, qualified name, embed-text header) derive from the
            // path relative to the project root, so a flat layout re-keys them.
            let path = lf_root.path().join("tests/fixtures").join(name);
            std::fs::create_dir_all(path.parent().unwrap()).expect("fixture dirs");
            std::fs::write(&path, source).expect("write LF fixture");
            path
        });
        let project_root = lf_root.path();
        let (chunks, _) =
            SemanticIndex::collect_chunks(project_root, &fixture_files, EmbedTextCaps::default());
        let normalized = chunks
            .iter()
            .map(|chunk| {
                (
                    chunk
                        .file
                        .strip_prefix(project_root)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/"),
                    &chunk.name,
                    &chunk.qualified_name,
                    &chunk.kind,
                    chunk.start_line,
                    chunk.end_line,
                    chunk.exported,
                    &chunk.embed_text,
                    &chunk.snippet,
                )
            })
            .collect::<Vec<_>>();
        let output = format!("{normalized:#?}");
        (
            chunks.len(),
            output.len(),
            blake3::hash(output.as_bytes()).to_hex().to_string(),
        )
    }

    // Unix-only: chunk embed text bakes the OS-native relative path into its
    // header (file-summary chunks), so a Windows run hashes "tests\fixtures\…"
    // and can never reproduce the unix-captured baseline even with LF-forced
    // sources. The property under test — the query-free Rust walk reproduces
    // the old RS_QUERY output byte-for-byte — is platform-independent and is
    // pinned where the baseline was captured.
    #[cfg(unix)]
    #[test]
    fn rust_semantic_fixture_output_matches_query_baseline() {
        let project_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let (_, _, output_hash) = rust_fixture_semantic_output_fingerprint(&project_root);
        assert_eq!(output_hash, RUST_QUERY_BASELINE_OUTPUT_HASH);
    }

    #[test]
    #[ignore = "manual single-file semantic collect phase benchmark"]
    fn profile_rust_single_file_semantic_collect() {
        let crate_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let workspace_root = crate_root
            .parent()
            .and_then(Path::parent)
            .expect("workspace root");
        let files = [
            workspace_root.join("crates/aft/src/bash_background/registry.rs"),
            workspace_root.join("crates/aft-tokenizer/src/claude_data.rs"),
        ];

        for file in files {
            let source = fs::read_to_string(&file).expect("read benchmark source");
            for run in 1..=5 {
                let mut phases = SemanticCollectPhaseTimings::default();
                let started = Instant::now();
                let chunks = collect_file_chunks_from_source_timed(
                    workspace_root,
                    &file,
                    crate::parser::LangId::Rust,
                    &source,
                    EmbedTextCaps::default(),
                    &mut phases,
                )
                .unwrap();
                eprintln!(
                    "semantic single-file file={} bytes={} run={run}: total={:?} parse={:?} extract={:?} build={:?} chunks={}",
                    file.strip_prefix(workspace_root).unwrap().display(),
                    source.len(),
                    started.elapsed(),
                    phases.parse,
                    phases.extract,
                    phases.build,
                    chunks.len()
                );
            }
        }
    }

    #[test]
    fn semantic_index_includes_php_inc_and_scss_extensions() {
        for file in ["partial.inc", "index.php", "styles.scss"] {
            assert!(
                is_semantic_indexed_extension(Path::new(file)),
                "{file} should be semantic-index eligible"
            );
        }
    }

    #[test]
    fn semantic_index_includes_groovy_extensions_and_jenkinsfile() {
        for file in [
            "script.groovy",
            "script.gvy",
            "script.gy",
            "shell.gsh",
            "build.gradle",
            "Jenkinsfile",
        ] {
            assert!(
                is_semantic_indexed_extension(Path::new(file)),
                "{file} should be semantic-index eligible"
            );
        }
        assert!(is_semantic_indexed_extension(Path::new("build.gradle.kts")));
    }

    #[test]
    fn transient_marker_round_trips_and_classifies() {
        // A marked transient error is recognized and the marker is stripped for
        // display, leaving a clean message.
        let marked = format!("{TRANSIENT_EMBEDDING_MARKER}openai compatible request failed: error sending request for url (http://localhost:1234/v1/embeddings)");
        assert!(embedding_failure_is_transient(&marked));
        let clean = strip_transient_embedding_marker(&marked);
        assert!(!clean.contains(TRANSIENT_EMBEDDING_MARKER));
        assert!(clean.starts_with("openai compatible request failed:"));

        // Permanent errors (HTTP 4xx, dimension mismatch) carry no marker and
        // are not classified transient — they must fail fast.
        for permanent in [
            "openai compatible request failed (HTTP 401): Unauthorized",
            "embedding dimension mismatch: index has 384, model returned 768",
            "too many files (>20000) for semantic indexing (max 20000)",
        ] {
            assert!(
                !embedding_failure_is_transient(permanent),
                "{permanent:?} must not be transient"
            );
            // Stripping a marker-free string is a no-op.
            assert_eq!(strip_transient_embedding_marker(permanent), permanent);
        }
    }

    #[test]
    fn send_error_transience_separates_connect_timeout_from_4xx() {
        // 5xx / 429 are transient; other client errors are not.
        assert!(is_retryable_embedding_status(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR
        ));
        assert!(is_retryable_embedding_status(
            reqwest::StatusCode::TOO_MANY_REQUESTS
        ));
        assert!(!is_retryable_embedding_status(
            reqwest::StatusCode::UNAUTHORIZED
        ));
        assert!(!is_retryable_embedding_status(
            reqwest::StatusCode::BAD_REQUEST
        ));
    }

    #[test]
    fn query_timeout_marker_round_trips_and_classifies() {
        // A query-timeout error carries the budget that fired; the budget is
        // recoverable and the marker strips cleanly for display.
        let marked = format!(
            "{}openai compatible request failed: operation timed out",
            query_embedding_timeout_marker(3_000)
        );
        assert_eq!(query_embedding_timeout_budget(&marked), Some(3_000));
        let clean = strip_query_embedding_timeout_marker(&marked);
        assert!(!clean.contains(QUERY_EMBEDDING_TIMEOUT_MARKER_PREFIX));
        assert!(clean.starts_with("openai compatible request failed:"));

        // Non-timeout errors carry no marker and no budget — they must not be
        // misclassified as timeouts.
        for permanent in [
            "openai compatible request failed (HTTP 401): Unauthorized",
            "failed to embed query: embedding model was not initialized",
            "openai compatible request failed: connection refused",
        ] {
            assert_eq!(
                query_embedding_timeout_budget(permanent),
                None,
                "{permanent:?} must not classify as a query timeout"
            );
            assert_eq!(
                strip_query_embedding_timeout_marker(permanent),
                permanent,
                "stripping a marker-free string is a no-op"
            );
        }
    }

    fn install_test_crypto_provider() {
        // Reqwest and the direct test-server dependency enable different rustls
        // providers, so select one explicitly before either side builds TLS.
        let _ = rustls::crypto::ring::default_provider().install_default();
    }

    fn start_platform_verifier_tls_server() -> (String, NamedTempFile, thread::JoinHandle<()>) {
        install_test_crypto_provider();
        let ca_key = rcgen::KeyPair::generate().expect("generate test CA key");
        let mut ca_params = rcgen::CertificateParams::default();
        ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        ca_params.key_usages = vec![
            rcgen::KeyUsagePurpose::KeyCertSign,
            rcgen::KeyUsagePurpose::DigitalSignature,
        ];
        let ca_cert = ca_params
            .self_signed(&ca_key)
            .expect("generate test CA certificate");

        let leaf_key = rcgen::KeyPair::generate().expect("generate test leaf key");
        let mut leaf_params = rcgen::CertificateParams::new(vec!["localhost".to_string()])
            .expect("generate leaf parameters");
        leaf_params.key_usages = vec![rcgen::KeyUsagePurpose::DigitalSignature];
        leaf_params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
        let leaf_cert = leaf_params
            .signed_by(&leaf_key, &ca_cert, &ca_key)
            .expect("sign test leaf certificate");

        let mut ca_file = NamedTempFile::new().expect("create test CA file");
        ca_file
            .write_all(ca_cert.pem().as_bytes())
            .expect("write test CA certificate");

        let server_config = Arc::new(
            rustls::ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(
                    vec![rustls::pki_types::CertificateDer::from(
                        leaf_cert.der().to_vec(),
                    )],
                    rustls::pki_types::PrivateKeyDer::Pkcs8(
                        rustls::pki_types::PrivatePkcs8KeyDer::from(leaf_key.serialize_der()),
                    ),
                )
                .expect("build test TLS server configuration"),
        );
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind test TLS server");
        let address = listener.local_addr().expect("read test TLS server address");
        let url = format!("https://localhost:{}/v1/embeddings", address.port());
        let handle = thread::spawn(move || {
            // Linux exercises both trust paths: the first handshake fails with
            // UnknownIssuer, and the second succeeds after SSL_CERT_FILE supplies
            // the throwaway CA. Other platforms only exercise the failure path;
            // their platform verifiers do not consult SSL_CERT_FILE.
            let expected_connections = if cfg!(target_os = "linux") { 2 } else { 1 };
            for _ in 0..expected_connections {
                let (stream, _) = listener.accept().expect("accept test TLS connection");
                stream
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .expect("set test TLS read timeout");
                let connection = rustls::ServerConnection::new(server_config.clone())
                    .expect("create test TLS server connection");
                let mut tls_stream = rustls::StreamOwned::new(connection, stream);
                let mut request = [0_u8; 4096];
                if tls_stream.read(&mut request).is_ok() {
                    let body = r#"{"data":[],"model":"test","object":"list"}"#;
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(), body
                    );
                    let _ = tls_stream.write_all(response.as_bytes());
                    tls_stream.conn.send_close_notify();
                    let _ = tls_stream.flush();
                }
            }
        });

        (url, ca_file, handle)
    }

    fn run_platform_verifier_tls_child() {
        install_test_crypto_provider();
        let url = env::var("AFT_PLATFORM_VERIFIER_TLS_URL").expect("test TLS URL");
        let tls_config = crate::platform_tls::client_config().expect("build platform TLS config");
        // This test asserts the ERROR CLASS (certificate trust failure), not
        // latency, so the budget must be unreachable by keychain slowness: on
        // macOS the first evaluation of an untrusted chain walks user trust
        // settings (trustd), which a pathological keychain entry plus machine
        // load has stretched past 120s — at which point the request surfaces a
        // transient "operation timed out" BEFORE the certificate verdict
        // exists and the assertion fails on the wrong error class. 120s was
        // tried twice and breached twice (485s observed once under load ~100).
        // Clean keychains answer in milliseconds; this budget only ever costs
        // time on machines with hostile trust settings, where a slow correct
        // verdict beats a fast wrong one.
        let client = Client::builder()
            .timeout(Duration::from_secs(600))
            .use_preconfigured_tls(tls_config)
            .build()
            .expect("build test embedding client");
        let result = send_embedding_request(
            || client.post(&url).body("{}"),
            "openai compatible",
            EmbeddingRequestPolicy::Query(QueryBudget {
                timeout_ms: 600_000,
            }),
        );

        #[cfg(target_os = "linux")]
        if env::var_os("SSL_CERT_FILE").is_some() {
            let body = result.expect("SSL_CERT_FILE should make the private CA trusted");
            assert!(
                body.contains("\"data\""),
                "unexpected embedding response: {body}"
            );
            return;
        }

        let error = result.expect_err("the private CA must not be trusted on this path");
        let lower = error.to_ascii_lowercase();
        assert!(
            ["certificate", "unknownissuer", "unknown issuer", "trust"]
                .iter()
                .any(|marker| lower.contains(marker)),
            "the rendered source chain must include a certificate trust failure: {error}"
        );
        assert!(
            !embedding_failure_is_transient(&error),
            "certificate trust failures must not be retried: {error}"
        );
    }

    #[test]
    fn platform_verifier_tls_client_subprocess() {
        if env::var_os("AFT_PLATFORM_VERIFIER_TLS_CHILD").is_some() {
            run_platform_verifier_tls_child();
            return;
        }

        // Run each trust configuration in a fresh process because the
        // TLS/platform-verifier configuration caches CA settings; SSL_CERT_FILE
        // must be set before that configuration is initialized for Linux CA
        // discovery to use it. The process-env lock prevents this test from
        // racing other tests that modify environment variables. macOS and Windows
        // exercise only the untrusted path because their platform verifiers do
        // not consult SSL_CERT_FILE.
        let _env_lock = crate::test_env::process_env_lock();
        let (url, _ca_file, server_handle) = start_platform_verifier_tls_server();
        let test_name = "semantic_index::tests::platform_verifier_tls_client_subprocess";
        #[cfg(target_os = "linux")]
        let ca_paths: &[Option<&Path>] = &[None, Some(_ca_file.path())];
        #[cfg(not(target_os = "linux"))]
        let ca_paths: &[Option<&Path>] = &[None];

        for ca_path in ca_paths {
            let mut command = Command::new(env::current_exe().expect("test executable"));
            command
                .args(["--exact", test_name, "--nocapture"])
                .env("AFT_PLATFORM_VERIFIER_TLS_CHILD", "1")
                .env("AFT_PLATFORM_VERIFIER_TLS_URL", &url)
                .env_remove("SSL_CERT_FILE")
                .env_remove("SSL_CERT_DIR");
            if let Some(ca_path) = ca_path {
                command.env("SSL_CERT_FILE", ca_path);
            }
            let output = command.output().expect("run TLS child test");
            // Name the exit status and any terminating signal in the failure:
            // under heavy machine load this child has died with EMPTY output,
            // and a blind "child failed" leaves nothing to diagnose with.
            #[cfg(unix)]
            let signal = std::os::unix::process::ExitStatusExt::signal(&output.status);
            #[cfg(not(unix))]
            let signal: Option<i32> = None;
            assert!(
                output.status.success(),
                "TLS child failed: status={:?} code={:?} signal={:?}\nstdout:\n{}\nstderr:\n{}",
                output.status,
                output.status.code(),
                signal,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }

        server_handle.join().expect("join test TLS server");
    }

    #[test]
    fn local_backend_model_loading_body_is_transient() {
        // LM Studio / Ollama return a 4xx with a loading/unloaded message while
        // the model swaps; these must classify transient so the build self-heals.
        for body in [
            r#"{"error":"Model was unloaded while the request was still in queue.."}"#,
            r#"{"error":"model is loading, please wait"}"#,
            r#"{"error":"Model not loaded"}"#,
            "Loading model into memory",
        ] {
            assert!(
                embedding_response_body_is_transient(reqwest::StatusCode::BAD_REQUEST, body),
                "{body:?} should be body-transient"
            );
        }

        // A genuine 4xx misconfiguration body must NOT be treated as transient,
        // even when it happens to contain generic words from the old broad
        // substring matcher.
        for body in [
            r#"{"error":"invalid api key"}"#,
            r#"{"error":"model 'foo' not found"}"#,
            "Bad Request: unknown field",
            "Bad Request: invalid loading model option",
            r#"{"error":"unauthorized while model is being loaded by another account"}"#,
        ] {
            assert!(
                !embedding_response_body_is_transient(reqwest::StatusCode::BAD_REQUEST, body),
                "{body:?} must not be body-transient"
            );
        }

        assert!(
            !embedding_response_body_is_transient(
                reqwest::StatusCode::UNAUTHORIZED,
                r#"{"error":"model is loading, please wait"}"#
            ),
            "permanent auth failures must not become transient because of body text"
        );
    }

    #[test]
    fn context_overflow_body_classification_is_narrow_and_extracts_counts() {
        let fixtures = [
            (
                r#"{"error":{"type":"exceed_context_size_error","message":"input is too large to process","n_prompt_tokens":518,"n_ctx":512}}"#,
                Some(512),
                Some(518),
            ),
            (
                r#"{"error":{"type":"exceed_context_size_error","message":"input is too large to process"}}"#,
                None,
                None,
            ),
            (
                r#"{"error":{"message":"maximum context length is 8192 tokens; you requested 9000 tokens"}}"#,
                Some(8192),
                Some(9000),
            ),
            (
                r#"{"error":{"message":"This model's maximum context length is 4096 tokens. Your input resulted in 5000 tokens"}}"#,
                Some(4096),
                Some(5000),
            ),
            (
                r#"{"error":"input length exceeds model context"}"#,
                None,
                None,
            ),
        ];

        for (body, expected_limit, expected_actual) in fixtures {
            let details = embedding_response_row_too_long(reqwest::StatusCode::BAD_REQUEST, body)
                .unwrap_or_else(|| panic!("overflow fixture was not classified: {body}"));
            assert_eq!(details.limit_tokens, expected_limit, "body={body}");
            assert_eq!(details.actual_tokens, expected_actual, "body={body}");
        }

        assert_eq!(
            embedding_response_row_too_long(
                reqwest::StatusCode::BAD_REQUEST,
                r#"{"error":"model not found"}"#,
            ),
            None,
        );
        assert_eq!(
            embedding_response_row_too_long(
                reqwest::StatusCode::INTERNAL_SERVER_ERROR,
                r#"{"error":"input length exceeds model context"}"#,
            ),
            None,
        );

        let text = "name:dense file:src/dense.rs kind:function name:dense signature:fn dense() body:abcdefghij";
        assert_eq!(
            shrink_embed_text(
                text,
                RowTooLongDetails {
                    limit_tokens: None,
                    actual_tokens: None,
                },
            )
            .as_deref(),
            Some("name:dense file:src/dense.rs kind:function name:dense signature:fn dense() body:abcde"),
        );
        assert!(shrink_embed_text(
            "header-free-base64",
            RowTooLongDetails {
                limit_tokens: None,
                actual_tokens: None,
            },
        )
        .is_none());
    }

    fn start_slow_embedding_server(
        expected_requests: usize,
        response_delay: Duration,
    ) -> (String, Arc<AtomicUsize>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind slow embedding server");
        listener
            .set_nonblocking(true)
            .expect("set slow server nonblocking");
        let addr = listener.local_addr().expect("slow embedding server addr");
        let requests = Arc::new(AtomicUsize::new(0));
        let requests_for_thread = Arc::clone(&requests);
        let handle = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut handlers = Vec::new();
            while requests_for_thread.load(Ordering::SeqCst) < expected_requests
                && Instant::now() < deadline
            {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        requests_for_thread.fetch_add(1, Ordering::SeqCst);
                        handlers.push(thread::spawn(move || {
                            let mut request = [0u8; 4096];
                            let _ = stream.read(&mut request);
                            thread::sleep(response_delay);
                            let body =
                                r#"{"data":[{"embedding":[0.1,0.2,0.3],"index":0}]}"#;
                            let response = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                                body.len(),
                                body
                            );
                            let _ = stream.write_all(response.as_bytes());
                        }));
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("accept slow embedding request: {error}"),
                }
            }
            for handler in handlers {
                handler.join().expect("slow embedding handler");
            }
        });

        (format!("http://{addr}"), requests, handle)
    }

    struct ProgrammableEmbeddingServer {
        base_url: String,
        per_item_delay_ms: Arc<AtomicU64>,
        never_answer: Arc<AtomicBool>,
        requests: Arc<Mutex<Vec<usize>>>,
        completed: Arc<Mutex<Vec<usize>>>,
        shutdown: Arc<AtomicBool>,
        handle: Option<thread::JoinHandle<()>>,
    }

    impl ProgrammableEmbeddingServer {
        fn start(per_item_delay: Duration) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind programmable server");
            listener
                .set_nonblocking(true)
                .expect("set programmable server nonblocking");
            let addr = listener.local_addr().expect("programmable server addr");
            let per_item_delay_ms = Arc::new(AtomicU64::new(
                per_item_delay.as_millis().min(u128::from(u64::MAX)) as u64,
            ));
            let never_answer = Arc::new(AtomicBool::new(false));
            let requests = Arc::new(Mutex::new(Vec::new()));
            let completed = Arc::new(Mutex::new(Vec::new()));
            let shutdown = Arc::new(AtomicBool::new(false));
            let thread_delay = Arc::clone(&per_item_delay_ms);
            let thread_never = Arc::clone(&never_answer);
            let thread_requests = Arc::clone(&requests);
            let thread_completed = Arc::clone(&completed);
            let thread_shutdown = Arc::clone(&shutdown);
            let handle = thread::spawn(move || {
                let mut handlers = Vec::new();
                while !thread_shutdown.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let delay = Arc::clone(&thread_delay);
                            let never = Arc::clone(&thread_never);
                            let requests = Arc::clone(&thread_requests);
                            let completed = Arc::clone(&thread_completed);
                            let shutdown = Arc::clone(&thread_shutdown);
                            handlers.push(thread::spawn(move || {
                                handle_programmable_embedding_request(
                                    stream, delay, never, requests, completed, shutdown,
                                );
                            }));
                        }
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(2));
                        }
                        Err(error) => panic!("accept programmable embedding request: {error}"),
                    }
                }
                for handler in handlers {
                    handler.join().expect("programmable embedding handler");
                }
            });

            Self {
                base_url: format!("http://{addr}"),
                per_item_delay_ms,
                never_answer,
                requests,
                completed,
                shutdown,
                handle: Some(handle),
            }
        }

        fn set_per_item_delay(&self, delay: Duration) {
            self.per_item_delay_ms.store(
                delay.as_millis().min(u128::from(u64::MAX)) as u64,
                Ordering::SeqCst,
            );
        }

        fn set_never_answer(&self, never_answer: bool) {
            self.never_answer.store(never_answer, Ordering::SeqCst);
        }

        fn request_sizes(&self) -> Vec<usize> {
            self.requests.lock().unwrap().clone()
        }

        fn completed_sizes(&self) -> Vec<usize> {
            self.completed.lock().unwrap().clone()
        }
    }

    impl Drop for ProgrammableEmbeddingServer {
        fn drop(&mut self) {
            self.shutdown.store(true, Ordering::SeqCst);
            if let Some(handle) = self.handle.take() {
                handle.join().expect("programmable embedding server");
            }
        }
    }

    /// Close a fake server's connection the way a real HTTP server does: flush,
    /// half-close the write side, then drain whatever the client still sends
    /// until it closes. Dropping the socket with unread client bytes makes
    /// Windows answer with RST instead of FIN, and hyper then reports
    /// "connection aborted" (WSAECONNABORTED) before it has read the response
    /// (train 127, unknown_4xx_still_aborts_semantic_build).
    fn finish_test_response(mut stream: TcpStream) {
        let _ = stream.flush();
        let _ = stream.shutdown(std::net::Shutdown::Write);
        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
        let _ = stream.set_nonblocking(false);
        let mut sink = [0u8; 4096];
        while let Ok(count) = stream.read(&mut sink) {
            if count == 0 {
                break;
            }
        }
    }

    fn handle_programmable_embedding_request(
        mut stream: TcpStream,
        per_item_delay_ms: Arc<AtomicU64>,
        never_answer: Arc<AtomicBool>,
        requests: Arc<Mutex<Vec<usize>>>,
        completed: Arc<Mutex<Vec<usize>>>,
        shutdown: Arc<AtomicBool>,
    ) {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        let mut header_end = None;
        let mut content_length = 0usize;
        loop {
            let count = match stream.read(&mut chunk) {
                Ok(count) => count,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(1));
                    continue;
                }
                Err(error) => panic!("read programmable request: {error}"),
            };
            if count == 0 {
                return;
            }
            buf.extend_from_slice(&chunk[..count]);
            if header_end.is_none() {
                if let Some(position) = buf.windows(4).position(|window| window == b"\r\n\r\n") {
                    header_end = Some(position + 4);
                    for line in String::from_utf8_lossy(&buf[..position + 4]).lines() {
                        if line.to_ascii_lowercase().starts_with("content-length:") {
                            content_length = line
                                .split_once(':')
                                .and_then(|(_, value)| value.trim().parse().ok())
                                .unwrap_or(0);
                        }
                    }
                }
            }
            if header_end.is_some_and(|end| buf.len() >= end + content_length) {
                break;
            }
        }
        let body_start = header_end.expect("programmable request headers");
        let body: serde_json::Value =
            serde_json::from_slice(&buf[body_start..body_start + content_length])
                .expect("programmable request JSON");
        let input_count = body["input"]
            .as_array()
            .expect("embedding input array")
            .len();
        requests.lock().unwrap().push(input_count);

        if never_answer.load(Ordering::SeqCst) {
            while !shutdown.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(2));
            }
            return;
        }

        thread::sleep(Duration::from_millis(
            per_item_delay_ms
                .load(Ordering::SeqCst)
                .saturating_mul(input_count as u64),
        ));
        let data = (0..input_count)
            .map(|index| serde_json::json!({"embedding": [0.1, 0.2, 0.3], "index": index}))
            .collect::<Vec<_>>();
        let response_body = serde_json::json!({"data": data}).to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            response_body.len(),
            response_body,
        );
        if stream.write_all(response.as_bytes()).is_ok() {
            completed.lock().unwrap().push(input_count);
        }
        finish_test_response(stream);
    }

    enum TestEmbeddingRejection {
        Oversize { max_bytes: usize },
        Always { body: String },
    }

    struct OverflowEmbeddingServer {
        base_url: String,
        requests: Arc<Mutex<Vec<Vec<String>>>>,
        shutdown: Arc<AtomicBool>,
        handle: Option<thread::JoinHandle<()>>,
    }

    impl OverflowEmbeddingServer {
        fn rejecting_oversize(max_bytes: usize) -> Self {
            Self::start(TestEmbeddingRejection::Oversize { max_bytes })
        }

        fn rejecting_all(body: impl Into<String>) -> Self {
            Self::start(TestEmbeddingRejection::Always { body: body.into() })
        }

        fn start(rejection: TestEmbeddingRejection) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind overflow server");
            listener
                .set_nonblocking(true)
                .expect("set overflow server nonblocking");
            let addr = listener.local_addr().expect("overflow server addr");
            let requests = Arc::new(Mutex::new(Vec::new()));
            let requests_for_thread = Arc::clone(&requests);
            let shutdown = Arc::new(AtomicBool::new(false));
            let shutdown_for_thread = Arc::clone(&shutdown);
            let handle = thread::spawn(move || {
                while !shutdown_for_thread.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, _)) => handle_overflow_embedding_request(
                            stream,
                            &rejection,
                            &requests_for_thread,
                        ),
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(1));
                        }
                        Err(error) => panic!("accept overflow request: {error}"),
                    }
                }
            });
            Self {
                base_url: format!("http://{addr}"),
                requests,
                shutdown,
                handle: Some(handle),
            }
        }

        fn requests(&self) -> Vec<Vec<String>> {
            self.requests.lock().unwrap().clone()
        }
    }

    impl Drop for OverflowEmbeddingServer {
        fn drop(&mut self) {
            self.shutdown.store(true, Ordering::SeqCst);
            if let Some(handle) = self.handle.take() {
                handle.join().expect("overflow embedding server");
            }
        }
    }

    fn handle_overflow_embedding_request(
        mut stream: TcpStream,
        rejection: &TestEmbeddingRejection,
        requests: &Arc<Mutex<Vec<Vec<String>>>>,
    ) {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        let mut header_end = None;
        let mut content_length = 0usize;
        loop {
            let Ok(count) = stream.read(&mut chunk) else {
                return;
            };
            if count == 0 {
                return;
            }
            buf.extend_from_slice(&chunk[..count]);
            if header_end.is_none() {
                if let Some(position) = buf.windows(4).position(|window| window == b"\r\n\r\n") {
                    header_end = Some(position + 4);
                    for line in String::from_utf8_lossy(&buf[..position + 4]).lines() {
                        if line.to_ascii_lowercase().starts_with("content-length:") {
                            content_length = line
                                .split_once(':')
                                .and_then(|(_, value)| value.trim().parse().ok())
                                .unwrap_or(0);
                        }
                    }
                }
            }
            if header_end.is_some_and(|end| buf.len() >= end + content_length) {
                break;
            }
        }

        let body_start = header_end.expect("overflow request headers");
        let body: serde_json::Value =
            serde_json::from_slice(&buf[body_start..body_start + content_length])
                .expect("overflow request JSON");
        let inputs = body["input"]
            .as_array()
            .expect("embedding input array")
            .iter()
            .map(|value| value.as_str().expect("embedding input text").to_string())
            .collect::<Vec<_>>();
        requests.lock().unwrap().push(inputs.clone());

        let rejected = match rejection {
            TestEmbeddingRejection::Oversize { max_bytes } => inputs
                .iter()
                .map(String::len)
                .max()
                .filter(|actual| actual > max_bytes)
                .map(|actual| {
                    serde_json::json!({
                        "error": {
                            "type": "exceed_context_size_error",
                            "message": "input is too large to process",
                            "n_prompt_tokens": actual,
                            "n_ctx": max_bytes,
                        }
                    })
                    .to_string()
                }),
            TestEmbeddingRejection::Always { body } => Some(body.clone()),
        };

        let (status, response_body) = if let Some(body) = rejected {
            ("400 Bad Request", body)
        } else {
            let data = inputs
                .iter()
                .enumerate()
                .map(|(index, text)| {
                    serde_json::json!({
                        "embedding": [text.len() as f32, 1.0, 0.5],
                        "index": index,
                    })
                })
                .collect::<Vec<_>>();
            ("200 OK", serde_json::json!({"data": data}).to_string())
        };
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            response_body.len(),
            response_body,
        );
        let _ = stream.write_all(response.as_bytes());
        finish_test_response(stream);
    }

    fn start_recording_embedding_server(
        expected_requests: usize,
    ) -> (String, Arc<Mutex<Vec<String>>>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind recording server");
        let addr = listener.local_addr().expect("recording server addr");
        let inputs = Arc::new(Mutex::new(Vec::new()));
        let inputs_for_thread = Arc::clone(&inputs);
        let handle = thread::spawn(move || {
            for _ in 0..expected_requests {
                let (mut stream, _) = listener.accept().expect("accept recording request");
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let mut header_end = None;
                let mut content_length = 0usize;
                loop {
                    let count = stream.read(&mut chunk).expect("read recording request");
                    if count == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..count]);
                    if header_end.is_none() {
                        if let Some(position) =
                            buf.windows(4).position(|window| window == b"\r\n\r\n")
                        {
                            header_end = Some(position + 4);
                            for line in String::from_utf8_lossy(&buf[..position + 4]).lines() {
                                if line.to_ascii_lowercase().starts_with("content-length:") {
                                    content_length = line
                                        .split_once(':')
                                        .map(|(_, value)| value.trim().parse().unwrap_or(0))
                                        .unwrap_or(0);
                                }
                            }
                        }
                    }
                    if header_end.is_some_and(|end| buf.len() >= end + content_length) {
                        break;
                    }
                }
                let body_start = header_end.expect("recording request headers");
                let body: serde_json::Value =
                    serde_json::from_slice(&buf[body_start..body_start + content_length])
                        .expect("recording request JSON");
                let input = body["input"][0]
                    .as_str()
                    .expect("single string embedding input")
                    .to_string();
                inputs_for_thread.lock().unwrap().push(input);
                let response_body = r#"{"data":[{"embedding":[0.1,0.2,0.3],"index":0}]}"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response_body.len(),
                    response_body
                );
                stream
                    .write_all(response.as_bytes())
                    .expect("write recording response");
            }
        });
        (format!("http://{addr}"), inputs, handle)
    }

    fn start_mock_http_server<F>(handler: F) -> (String, thread::JoinHandle<()>)
    where
        F: Fn(String, String, String) -> String + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let addr = listener.local_addr().expect("local addr");
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept request");
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            let mut header_end = None;
            let mut content_length = 0usize;
            loop {
                let n = stream.read(&mut chunk).expect("read request");
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                if header_end.is_none() {
                    if let Some(pos) = buf.windows(4).position(|window| window == b"\r\n\r\n") {
                        header_end = Some(pos + 4);
                        let headers = String::from_utf8_lossy(&buf[..pos + 4]);
                        for line in headers.lines() {
                            if let Some(value) = line.strip_prefix("Content-Length:") {
                                content_length = value.trim().parse::<usize>().unwrap_or(0);
                            }
                        }
                    }
                }
                if let Some(end) = header_end {
                    if buf.len() >= end + content_length {
                        break;
                    }
                }
            }

            let end = header_end.expect("header terminator");
            let request = String::from_utf8_lossy(&buf[..end]).to_string();
            let body = String::from_utf8_lossy(&buf[end..end + content_length]).to_string();
            let mut lines = request.lines();
            let request_line = lines.next().expect("request line").to_string();
            let path = request_line
                .split_whitespace()
                .nth(1)
                .expect("request path")
                .to_string();
            let response_body = handler(request_line, path, body);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            );
            stream
                .write_all(response.as_bytes())
                .expect("write response");
        });

        (format!("http://{}", addr), handle)
    }

    fn start_truncated_body_server(attempts: usize) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind truncated test server");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let addr = listener.local_addr().expect("local addr");
        let handle = thread::spawn(move || {
            // The deadline is only a hang-backstop for the case where the client
            // makes FEWER than `attempts` connections. It MUST comfortably exceed
            // the client's full retry budget (3 attempts: 3x250ms read-timeouts +
            // 500ms + 1000ms backoffs ~= 2.25s) so the last connect is always
            // accepted — otherwise the 3rd connect lands after a too-short
            // deadline, the server thread is already gone, and the client gets a
            // connect error ("request failed") instead of the body-read error the
            // test asserts. Under loaded CI (esp. Windows) thread scheduling
            // drifts the connects later, so this needs generous headroom.
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            let mut accepted = 0usize;
            while accepted < attempts && std::time::Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        accepted += 1;
                        let mut buf = [0u8; 4096];
                        // The client (under test) uses a 250ms timeout and drops
                        // the connection when the truncated body never completes.
                        // On Windows that disconnect surfaces as a hard socket
                        // error (WSAECONNRESET) on these read/write calls, where
                        // Unix returns a clean EOF. Tolerate both: the mock does
                        // not need the request bytes, and a write to an
                        // already-hung-up client is expected.
                        let _ = stream.read(&mut buf);
                        let response = "HTTP/1.1 200 OK
Content-Type: application/json
Content-Length: 128
Connection: close

{";
                        let _ = stream.write_all(response.as_bytes());
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("accept request: {error}"),
                }
            }
        });

        (format!("http://{}", addr), handle)
    }

    #[test]
    fn response_body_read_failures_are_marked_transient() {
        let (url, handle) = start_truncated_body_server(EMBEDDING_REQUEST_MAX_ATTEMPTS);
        // Generous client timeout: this test classifies BODY-TRUNCATION errors,
        // and a tight budget flips the failure into a connect/send timeout on a
        // loaded machine, changing which error string the assertions see.
        let client = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .expect("client");

        let error = send_embedding_request(
            || client.post(&url).body("{}"),
            "test backend",
            EmbeddingRequestPolicy::Build(BuildRequestBudget {
                batch_size: 1,
                deadline_ms: 250,
            }),
        )
        .expect_err("truncated body should fail");

        handle.join().unwrap();
        assert!(
            embedding_failure_is_transient(&error),
            "body read failures should be transient-marked: {error}"
        );
        // The mock closes the socket after writing a truncated body. Whether
        // the client observes that as a body-read EOF, as a send-stage
        // connection reset, or as hyper's UnexpectedMessage (the partial reply
        // arrived while the request was still being written) is an OS-level
        // race (Windows sends RST when the socket closes with unread request
        // bytes, and under load the mock's single read can return early). All
        // shapes are the backend dying mid-exchange and all must carry the
        // transient marker; the message prefix differs by stage.
        assert!(
            error.contains("response read failed") || error.contains("request failed"),
            "unexpected error shape: {error}"
        );
    }

    fn test_vector_for_texts(texts: Vec<String>) -> Result<Vec<Vec<f32>>, String> {
        Ok(texts.iter().map(|_| vec![1.0, 0.0, 0.0]).collect())
    }

    fn write_rust_file(path: &Path, function_name: &str) {
        fs::write(
            path,
            format!("pub fn {function_name}() -> bool {{\n    true\n}}\n"),
        )
        .unwrap();
    }

    fn build_test_index(project_root: &Path, files: &[PathBuf]) -> SemanticIndex {
        let mut embed = test_vector_for_texts;
        SemanticIndex::build(project_root, files, &mut embed, 8).unwrap()
    }

    fn test_project_root() -> PathBuf {
        std::env::current_dir().unwrap()
    }

    #[test]
    fn empty_snapshot_replaces_nonempty_and_loads_as_valid_tombstone() {
        let project = tempfile::tempdir().expect("create project");
        let storage = tempfile::tempdir().expect("create storage");
        let source = project.path().join("lib.rs");
        write_rust_file(&source, "persisted_symbol");
        let populated = build_test_index(project.path(), std::slice::from_ref(&source));
        assert!(populated.write_to_disk(storage.path(), "project"));

        let data_path = storage.path().join("semantic/project/semantic.bin");
        let populated_bytes = fs::read(&data_path).expect("read populated snapshot");
        let empty = SemanticIndex::new(project.path().to_path_buf(), populated.dimension());
        assert!(empty.write_to_disk(storage.path(), "project"));
        let empty_bytes = fs::read(&data_path).expect("read explicit empty snapshot");
        assert_ne!(empty_bytes, populated_bytes);
        let decoded = SemanticIndex::from_bytes(&empty_bytes, project.path())
            .expect("decode explicit empty snapshot");
        assert_eq!(decoded.entry_count(), 0);
        for _ in 0..2 {
            let loaded = SemanticIndex::read_from_disk(
                storage.path(),
                "project",
                project.path(),
                false,
                None,
            )
            .expect("explicit empty snapshot remains loadable");
            assert_eq!(loaded.entry_count(), 0);
        }
    }

    #[test]
    fn persistence_failure_is_reported_to_caller() {
        let project = tempfile::tempdir().expect("create project");
        let storage_parent = tempfile::tempdir().expect("create storage parent");
        let storage_file = storage_parent.path().join("not-a-directory");
        fs::write(&storage_file, b"occupied").expect("create blocking file");
        let empty = SemanticIndex::new(project.path().to_path_buf(), 3);

        assert!(!empty.write_to_disk(&storage_file, "project"));
    }

    #[test]
    fn semantic_memory_estimate_is_zero_when_empty_and_scales_with_entries() {
        let root = test_project_root();
        let mut index = SemanticIndex::new(root.clone(), 3);
        assert_eq!(index.estimated_memory().estimated_bytes, Some(0));

        let entry = |name: &str| EmbeddingEntry {
            chunk: SemanticChunk {
                file: root.join(format!("{name}.rs")),
                name: name.to_string(),
                qualified_name: Some(format!("module::{name}")),
                kind: SymbolKind::Function,
                start_line: 0,
                end_line: 1,
                exported: true,
                embed_text: format!("function {name} body"),
                snippet: format!("fn {name}() {{}}"),
            },
            norm: vector_norm(&[1.0, 2.0, 3.0]),
            vector: vec![1.0, 2.0, 3.0],
        };
        index.entries.push(entry("one"));
        let one_entry = index.estimated_memory().estimated_bytes.unwrap();
        assert!(one_entry > 0);
        index.entries.push(entry("two"));
        let two_entries = index.estimated_memory().estimated_bytes.unwrap();
        assert!(two_entries > one_entry);
    }

    fn set_file_metadata(index: &mut SemanticIndex, file: &Path, mtime: SystemTime, size: u64) {
        index.file_mtimes.insert(file.to_path_buf(), mtime);
        index.file_sizes.insert(file.to_path_buf(), size);
        index
            .file_hashes
            .insert(file.to_path_buf(), cache_freshness::zero_hash());
    }

    fn legacy_semantic_index_bytes(index: &SemanticIndex) -> Vec<u8> {
        let mut buf = Vec::new();
        let fingerprint_bytes = index.fingerprint.as_ref().and_then(|fingerprint| {
            let encoded = fingerprint.as_string();
            if encoded.is_empty() {
                None
            } else {
                Some(encoded.into_bytes())
            }
        });
        let file_mtimes: Vec<_> = index
            .file_mtimes
            .iter()
            .filter_map(|(path, mtime)| {
                cache_relative_path(&index.project_root, path)
                    .map(|relative| (relative, path, mtime))
            })
            .collect();
        let entries: Vec<_> = index
            .entries
            .iter()
            .filter_map(|entry| {
                cache_relative_path(&index.project_root, &entry.chunk.file)
                    .map(|relative| (relative, entry))
            })
            .collect();

        buf.push(SEMANTIC_INDEX_VERSION_V6);
        buf.extend_from_slice(&(index.dimension as u32).to_le_bytes());
        buf.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        let fp_bytes_ref: &[u8] = fingerprint_bytes.as_deref().unwrap_or(&[]);
        buf.extend_from_slice(&(fp_bytes_ref.len() as u32).to_le_bytes());
        buf.extend_from_slice(fp_bytes_ref);

        buf.extend_from_slice(&(file_mtimes.len() as u32).to_le_bytes());
        for (relative, path, mtime) in &file_mtimes {
            let path_bytes = relative.to_string_lossy().as_bytes().to_vec();
            buf.extend_from_slice(&(path_bytes.len() as u32).to_le_bytes());
            buf.extend_from_slice(&path_bytes);
            let duration = mtime
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default();
            buf.extend_from_slice(&duration.as_secs().to_le_bytes());
            buf.extend_from_slice(&duration.subsec_nanos().to_le_bytes());
            let size = index.file_sizes.get(*path).copied().unwrap_or_default();
            buf.extend_from_slice(&size.to_le_bytes());
            let hash = index
                .file_hashes
                .get(*path)
                .copied()
                .unwrap_or_else(cache_freshness::zero_hash);
            buf.extend_from_slice(hash.as_bytes());
        }

        for (relative, entry) in &entries {
            let c = &entry.chunk;
            let file_bytes = relative.to_string_lossy().as_bytes().to_vec();
            buf.extend_from_slice(&(file_bytes.len() as u32).to_le_bytes());
            buf.extend_from_slice(&file_bytes);

            let name_bytes = c.name.as_bytes();
            buf.extend_from_slice(&(name_bytes.len() as u32).to_le_bytes());
            buf.extend_from_slice(name_bytes);

            buf.push(symbol_kind_to_u8(&c.kind));
            buf.extend_from_slice(&(c.start_line as u32).to_le_bytes());
            buf.extend_from_slice(&(c.end_line as u32).to_le_bytes());
            buf.push(c.exported as u8);

            let snippet_bytes = c.snippet.as_bytes();
            buf.extend_from_slice(&(snippet_bytes.len() as u32).to_le_bytes());
            buf.extend_from_slice(snippet_bytes);

            let embed_bytes = c.embed_text.as_bytes();
            buf.extend_from_slice(&(embed_bytes.len() as u32).to_le_bytes());
            buf.extend_from_slice(embed_bytes);

            for &val in &entry.vector {
                buf.extend_from_slice(&val.to_le_bytes());
            }
        }

        buf
    }

    #[derive(Default)]
    struct RecordingEmbedder {
        calls: Vec<Vec<String>>,
    }

    impl RecordingEmbedder {
        fn embed(&mut self, texts: Vec<String>) -> Result<Vec<Vec<f32>>, String> {
            let vectors = texts
                .iter()
                .map(|text| deterministic_test_vector(text))
                .collect();
            self.calls.push(texts);
            Ok(vectors)
        }

        fn total_embedded_texts(&self) -> usize {
            self.calls.iter().map(Vec::len).sum()
        }

        fn embedded_texts(&self) -> Vec<&str> {
            self.calls
                .iter()
                .flat_map(|batch| batch.iter().map(String::as_str))
                .collect()
        }
    }

    fn deterministic_test_vector(text: &str) -> Vec<f32> {
        let hash = blake3::hash(text.as_bytes());
        let bytes = hash.as_bytes();
        vec![
            1.0,
            bytes[0] as f32 / 255.0,
            bytes[1] as f32 / 255.0,
            bytes[2] as f32 / 255.0,
        ]
    }

    fn build_recorded_test_index(project_root: &Path, files: &[PathBuf]) -> SemanticIndex {
        let mut embedder = RecordingEmbedder::default();
        let mut embed = |texts: Vec<String>| embedder.embed(texts);
        SemanticIndex::build(project_root, files, &mut embed, 16).unwrap()
    }

    fn force_stale(index: &mut SemanticIndex, file: &Path) {
        set_file_metadata(index, file, SystemTime::UNIX_EPOCH, 0);
    }

    fn write_source(path: &Path, source: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, source).unwrap();
    }

    fn entries_for_file<'a>(index: &'a SemanticIndex, file: &Path) -> Vec<&'a EmbeddingEntry> {
        index
            .entries
            .iter()
            .filter(|entry| entry.chunk.file == file)
            .collect()
    }

    fn entry_by_name<'a>(index: &'a SemanticIndex, file: &Path, name: &str) -> &'a EmbeddingEntry {
        index
            .entries
            .iter()
            .find(|entry| entry.chunk.file == file && entry.chunk.name == name)
            .unwrap_or_else(|| panic!("missing semantic entry {name} in {}", file.display()))
    }

    fn file_summary_entry<'a>(index: &'a SemanticIndex, file: &Path) -> &'a EmbeddingEntry {
        index
            .entries
            .iter()
            .find(|entry| entry.chunk.file == file && entry.chunk.kind == SymbolKind::FileSummary)
            .unwrap_or_else(|| panic!("missing file-summary entry in {}", file.display()))
    }

    #[test]
    fn borrowed_snapshots_deserialize_once_share_memory_and_drop_with_last_holder() {
        let owner = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let borrower_a = tempfile::tempdir().unwrap();
        let borrower_b = tempfile::tempdir().unwrap();
        let relative = Path::new("src/lib.rs");
        for root in [owner.path(), borrower_a.path(), borrower_b.path()] {
            let file = root.join(relative);
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(&file, "pub fn shared_symbol() -> bool { true }\n").unwrap();
        }
        let owner_file = owner.path().join(relative);
        let metadata = fs::metadata(&owner_file).unwrap();
        let mut index = SemanticIndex::new(owner.path().to_path_buf(), 3);
        index.entries.push(EmbeddingEntry {
            chunk: SemanticChunk {
                file: owner_file.clone(),
                name: "shared_symbol".to_string(),
                qualified_name: None,
                kind: SymbolKind::Function,
                start_line: 0,
                end_line: 0,
                exported: true,
                embed_text: "shared symbol".to_string(),
                snippet: "pub fn shared_symbol() -> bool { true }".to_string(),
            },
            norm: vector_norm(&[1.0, 0.0, 0.0]),
            vector: vec![1.0, 0.0, 0.0],
        });
        index.file_mtimes.insert(
            owner_file.clone(),
            metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        );
        index.file_sizes.insert(owner_file.clone(), metadata.len());
        index.file_hashes.insert(
            owner_file,
            blake3::hash(b"pub fn shared_symbol() -> bool { true }\n"),
        );
        index.set_fingerprint(SemanticIndexFingerprint {
            backend: "test".to_string(),
            model: "shared-base".to_string(),
            base_url: FALLBACK_BACKEND.to_string(),
            dimension: 3,
            chunking_version: default_chunking_version(),
            ..Default::default()
        });
        assert!(index.shared_base.is_none(), "owner indexes stay private");

        let project_key = format!(
            "shared-base-{}",
            blake3::hash(owner.path().as_os_str().as_encoded_bytes()).to_hex()
        );
        let dir = storage.path().join("semantic").join(&project_key);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("semantic.bin"), index.to_bytes()).unwrap();
        let loads_before = SHARED_SEMANTIC_BASE_LOADS.load(Ordering::Relaxed);
        let hits_before = SHARED_SEMANTIC_BASE_HITS.load(Ordering::Relaxed);
        let a = SemanticIndex::read_from_disk_borrow_tolerant(
            storage.path(),
            &project_key,
            borrower_a.path(),
        )
        .unwrap();
        let b = SemanticIndex::read_from_disk_borrow_tolerant(
            storage.path(),
            &project_key,
            borrower_b.path(),
        )
        .unwrap();
        let a_base = a.shared_base.as_ref().unwrap();
        let b_base = b.shared_base.as_ref().unwrap();
        assert!(Arc::ptr_eq(a_base, b_base));
        assert!(SHARED_SEMANTIC_BASE_LOADS.load(Ordering::Relaxed) > loads_before);
        assert!(SHARED_SEMANTIC_BASE_HITS.load(Ordering::Relaxed) > hits_before);
        assert_eq!(
            a.search(&[1.0, 0.0, 0.0], 1)[0].file,
            borrower_a.path().join(relative)
        );
        assert_eq!(
            b.search(&[1.0, 0.0, 0.0], 1)[0].file,
            borrower_b.path().join(relative)
        );
        assert_eq!(a.estimated_memory().estimated_bytes, Some(0));
        assert!(shared_semantic_bases_memory().estimated_bytes.unwrap_or(0) > 0);

        let weak = Arc::downgrade(a_base);
        let ctx = crate::context::AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                project_root: Some(borrower_a.path().to_path_buf()),
                ..crate::config::Config::default()
            },
        );
        *ctx.semantic_index()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(a);
        assert!(ctx.evict_idle_artifacts());
        assert!(
            weak.upgrade().is_some(),
            "the second borrower keeps the base live"
        );
        drop(b);
        assert!(
            weak.upgrade().is_none(),
            "the last borrower releases the base"
        );
    }

    #[test]
    fn borrowed_snapshot_hash_change_falls_back_to_private_copy() {
        let owner = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let borrower_a = tempfile::tempdir().unwrap();
        let borrower_b = tempfile::tempdir().unwrap();
        let relative = Path::new("src/lib.rs");
        for root in [owner.path(), borrower_a.path(), borrower_b.path()] {
            let file = root.join(relative);
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(&file, "pub fn hash_guard() {}\n").unwrap();
        }
        let owner_file = owner.path().join(relative);
        let metadata = fs::metadata(&owner_file).unwrap();
        let mut index = SemanticIndex::new(owner.path().to_path_buf(), 2);
        index.entries.push(EmbeddingEntry {
            chunk: SemanticChunk {
                file: owner_file.clone(),
                name: "hash_guard".to_string(),
                qualified_name: None,
                kind: SymbolKind::Function,
                start_line: 0,
                end_line: 0,
                exported: true,
                embed_text: "hash guard".to_string(),
                snippet: "pub fn hash_guard() {}".to_string(),
            },
            norm: vector_norm(&[1.0, 0.0]),
            vector: vec![1.0, 0.0],
        });
        index.file_mtimes.insert(
            owner_file.clone(),
            metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        );
        index.file_sizes.insert(owner_file.clone(), metadata.len());
        index
            .file_hashes
            .insert(owner_file, blake3::hash(b"pub fn hash_guard() {}\n"));
        index.set_fingerprint(SemanticIndexFingerprint {
            backend: "test".to_string(),
            model: "hash-guard".to_string(),
            base_url: FALLBACK_BACKEND.to_string(),
            dimension: 2,
            chunking_version: default_chunking_version(),
            ..Default::default()
        });
        let project_key = format!(
            "hash-fallback-{}",
            blake3::hash(owner.path().as_os_str().as_encoded_bytes()).to_hex()
        );
        let dir = storage.path().join("semantic").join(&project_key);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("semantic.bin"), index.to_bytes()).unwrap();
        let shared = SemanticIndex::read_from_disk_borrow_tolerant(
            storage.path(),
            &project_key,
            borrower_a.path(),
        )
        .unwrap();
        assert!(shared.shared_base.is_some());

        let changed_vector = vec![0.0, 1.0];
        index.entries[0].norm = vector_norm(&changed_vector);
        index.entries[0].vector = changed_vector;
        fs::write(dir.join("semantic.bin"), index.to_bytes()).unwrap();
        let fallback = SemanticIndex::read_from_disk_borrow_tolerant(
            storage.path(),
            &project_key,
            borrower_b.path(),
        )
        .unwrap();
        assert!(
            fallback.shared_base.is_none(),
            "a different byte identity must not join the live shared generation"
        );
        drop(shared);
    }

    #[test]
    fn borrow_only_root_skips_semantic_lock_and_persist() {
        let project = tempfile::tempdir().expect("project");
        let source = project.path().join("lib.rs");
        write_rust_file(&source, "borrow_only_symbol");
        let project_key = "shared-artifact-key".to_string();
        let storage = tempfile::tempdir().expect("storage");
        crate::root_cache::configure_artifact_access(project.path(), &project_key, true);

        let _lock = SemanticIndexLock::acquire(storage.path(), &project_key, project.path())
            .expect("borrow-only lock downgrade");
        let cache_dir = storage.path().join("semantic").join(&project_key);
        assert!(!cache_dir.join("cache.lock").exists());

        let index = build_test_index(project.path(), &[source]);
        index.write_to_disk(storage.path(), &project_key);

        assert!(!cache_dir.join("semantic.bin").exists());
        assert!(!cache_dir.exists());
    }

    #[test]
    fn corpus_refresh_failure_reports_exact_file_set_for_recovery() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let changed = root.join("changed.rs");
        let deleted = root.join("deleted.rs");
        let unchanged = root.join("unchanged.rs");
        write_rust_file(&changed, "changed_before");
        write_rust_file(&deleted, "deleted");
        write_rust_file(&unchanged, "unchanged");
        let mut index = build_test_index(
            &root,
            &[changed.clone(), deleted.clone(), unchanged.clone()],
        );

        write_rust_file(&changed, "changed_after_with_a_longer_name");
        force_stale(&mut index, &changed);
        std::fs::remove_file(&deleted).unwrap();
        let added = root.join("added.rs");
        write_rust_file(&added, "added");
        let current_files = vec![changed.clone(), unchanged, added.clone()];
        let mut recovery_paths = Vec::new();
        let mut embed = |_texts: Vec<String>| -> Result<Vec<Vec<f32>>, String> {
            Err(format!("{TRANSIENT_EMBEDDING_MARKER}backend timeout"))
        };
        let mut progress = |_done: usize, _total: usize| {};

        let result = index.refresh_stale_files_with_strategy_and_blob_reuse(
            &root,
            &current_files,
            &mut embed,
            64,
            &mut progress,
            cache_freshness::VerifyStrategy::Strict,
            &mut |_| None,
            Some(&mut recovery_paths),
        );

        assert!(result.is_err());
        let mut expected = vec![added, changed, deleted];
        expected.sort();
        assert_eq!(recovery_paths, expected);
    }

    #[test]
    fn refresh_stale_line_shift_reuses_all_chunks_and_retains_entries() {
        let temp = tempfile::tempdir().unwrap();
        let project_root = temp.path();
        let file = project_root.join("src/lib.rs");
        let original = "pub fn alpha() -> i32 {\n    1\n}\n\npub fn beta() -> i32 {\n    2\n}\n";
        write_source(&file, original);

        let mut index = build_recorded_test_index(project_root, std::slice::from_ref(&file));
        let original_entry_count = index.entries.len();
        let original_alpha_vector = entry_by_name(&index, &file, "alpha").vector.clone();

        write_source(&file, &format!("\n{original}"));
        force_stale(&mut index, &file);

        let mut embedder = RecordingEmbedder::default();
        let mut embed = |texts: Vec<String>| embedder.embed(texts);
        let mut progress = |_done: usize, _total: usize| {};
        let summary = index
            .refresh_stale_files(
                project_root,
                std::slice::from_ref(&file),
                &mut embed,
                16,
                &mut progress,
            )
            .unwrap();

        assert_eq!(summary.changed, 1);
        assert_eq!(embedder.total_embedded_texts(), 0);
        assert_eq!(index.entries.len(), original_entry_count);
        let shifted_alpha = entry_by_name(&index, &file, "alpha");
        assert_eq!(shifted_alpha.chunk.start_line, 1);
        assert_eq!(shifted_alpha.vector, original_alpha_vector);
    }

    #[test]
    fn refresh_invalidated_line_shift_emits_full_replacement_delta_for_apply() {
        let temp = tempfile::tempdir().unwrap();
        let project_root = temp.path();
        let file = project_root.join("src/lib.rs");
        let original = "pub fn alpha() -> i32 {\n    1\n}\n\npub fn beta() -> i32 {\n    2\n}\n";
        write_source(&file, original);

        let mut worker_index = build_recorded_test_index(project_root, std::slice::from_ref(&file));
        let mut serving_index = worker_index.clone();
        let original_entry_count = worker_index.entries.len();

        write_source(&file, &format!("\n{original}"));

        let mut embedder = RecordingEmbedder::default();
        let mut embed = |texts: Vec<String>| embedder.embed(texts);
        let mut progress = |_done: usize, _total: usize| {};
        let update = worker_index
            .refresh_invalidated_files(
                project_root,
                std::slice::from_ref(&file),
                &mut embed,
                16,
                100,
                &mut progress,
            )
            .unwrap();

        assert_eq!(embedder.total_embedded_texts(), 0);
        assert_eq!(update.added_entries.len(), original_entry_count);
        assert_eq!(worker_index.entries.len(), original_entry_count);

        serving_index.apply_refresh_update(
            update.added_entries,
            update.updated_metadata,
            &update.completed_paths,
        );

        assert_eq!(serving_index.entries.len(), original_entry_count);
        assert_eq!(
            entries_for_file(&serving_index, &file).len(),
            original_entry_count
        );
        assert_eq!(
            entry_by_name(&serving_index, &file, "alpha")
                .chunk
                .start_line,
            1
        );
    }

    #[test]
    fn refresh_invalidated_one_symbol_edit_embeds_only_changed_symbol() {
        let temp = tempfile::tempdir().unwrap();
        let project_root = temp.path();
        let file = project_root.join("src/lib.rs");
        write_source(
            &file,
            "pub fn alpha() -> i32 {\n    1\n}\n\npub fn beta() -> i32 {\n    2\n}\n",
        );

        let mut index = build_recorded_test_index(project_root, std::slice::from_ref(&file));
        let original_entry_count = index.entries.len();
        let beta_vector = entry_by_name(&index, &file, "beta").vector.clone();

        write_source(
            &file,
            "pub fn alpha() -> i32 {\n    10\n}\n\npub fn beta() -> i32 {\n    2\n}\n",
        );

        let mut embedder = RecordingEmbedder::default();
        let mut embed = |texts: Vec<String>| embedder.embed(texts);
        let mut progress = |_done: usize, _total: usize| {};
        let update = index
            .refresh_invalidated_files(
                project_root,
                std::slice::from_ref(&file),
                &mut embed,
                16,
                100,
                &mut progress,
            )
            .unwrap();

        assert_eq!(embedder.total_embedded_texts(), 1);
        assert!(embedder.embedded_texts()[0].contains("name:alpha"));
        assert_eq!(update.added_entries.len(), original_entry_count);
        assert_eq!(entry_by_name(&index, &file, "beta").vector, beta_vector);
    }

    #[test]
    fn refresh_reuses_one_old_vector_for_two_byte_identical_symbols() {
        let temp = tempfile::tempdir().unwrap();
        let project_root = temp.path();
        let file = project_root.join("src/dupe.js");
        let one_duplicate = "function duplicate() {\n  return 1;\n}\n";
        write_source(&file, one_duplicate);

        let mut index = build_recorded_test_index(project_root, std::slice::from_ref(&file));
        let original_vector = entry_by_name(&index, &file, "duplicate").vector.clone();

        write_source(&file, &format!("{one_duplicate}\n{one_duplicate}"));

        let mut embedder = RecordingEmbedder::default();
        let mut embed = |texts: Vec<String>| embedder.embed(texts);
        let mut progress = |_done: usize, _total: usize| {};
        index
            .refresh_invalidated_files(
                project_root,
                std::slice::from_ref(&file),
                &mut embed,
                16,
                100,
                &mut progress,
            )
            .unwrap();

        let duplicate_entries = index
            .entries
            .iter()
            .filter(|entry| entry.chunk.file == file && entry.chunk.name == "duplicate")
            .collect::<Vec<_>>();
        assert_eq!(duplicate_entries.len(), 2);
        assert_eq!(embedder.total_embedded_texts(), 0);
        assert_eq!(duplicate_entries[0].vector, original_vector);
        assert_eq!(duplicate_entries[1].vector, original_vector);
    }

    #[test]
    fn file_summary_reuses_on_body_edit_and_misses_on_leading_doc_edit() {
        let temp = tempfile::tempdir().unwrap();
        let project_root = temp.path();
        let file = project_root.join("src/lib.rs");
        write_source(
            &file,
            "//! module docs v1\n\npub fn alpha() -> i32 {\n    1\n}\n",
        );

        let mut index = build_recorded_test_index(project_root, std::slice::from_ref(&file));
        let summary_before = file_summary_entry(&index, &file).vector.clone();

        write_source(
            &file,
            "//! module docs v1\n\npub fn alpha() -> i32 {\n    2\n}\n",
        );
        let mut body_embedder = RecordingEmbedder::default();
        let mut body_embed = |texts: Vec<String>| body_embedder.embed(texts);
        let mut progress = |_done: usize, _total: usize| {};
        index
            .refresh_invalidated_files(
                project_root,
                std::slice::from_ref(&file),
                &mut body_embed,
                16,
                100,
                &mut progress,
            )
            .unwrap();
        assert_eq!(body_embedder.total_embedded_texts(), 1);
        assert!(body_embedder.embedded_texts()[0].contains("name:alpha"));
        assert_eq!(file_summary_entry(&index, &file).vector, summary_before);

        write_source(
            &file,
            "//! module docs v2\n\npub fn alpha() -> i32 {\n    2\n}\n",
        );
        let mut doc_embedder = RecordingEmbedder::default();
        let mut doc_embed = |texts: Vec<String>| doc_embedder.embed(texts);
        index
            .refresh_invalidated_files(
                project_root,
                std::slice::from_ref(&file),
                &mut doc_embed,
                16,
                100,
                &mut progress,
            )
            .unwrap();

        assert_eq!(doc_embedder.total_embedded_texts(), 1);
        assert!(doc_embedder.embedded_texts()[0].contains("kind:file-summary"));
        assert_ne!(file_summary_entry(&index, &file).vector, summary_before);
    }

    #[test]
    fn refresh_invalidated_deleted_file_drops_entries_without_embedding() {
        let temp = tempfile::tempdir().unwrap();
        let project_root = temp.path();
        let file = project_root.join("src/lib.rs");
        write_source(&file, "pub fn alpha() -> i32 {\n    1\n}\n");

        let mut worker_index = build_recorded_test_index(project_root, std::slice::from_ref(&file));
        let mut serving_index = worker_index.clone();
        fs::remove_file(&file).unwrap();

        let mut embedder = RecordingEmbedder::default();
        let mut embed = |texts: Vec<String>| embedder.embed(texts);
        let mut progress = |_done: usize, _total: usize| {};
        let update = worker_index
            .refresh_invalidated_files(
                project_root,
                std::slice::from_ref(&file),
                &mut embed,
                16,
                100,
                &mut progress,
            )
            .unwrap();

        assert_eq!(update.summary.deleted, 1);
        assert_eq!(embedder.total_embedded_texts(), 0);
        assert!(worker_index.entries.is_empty());

        serving_index.apply_refresh_update(
            update.added_entries,
            update.updated_metadata,
            &update.completed_paths,
        );
        assert!(serving_index.entries.is_empty());
    }

    #[test]
    fn watcher_collect_failure_does_not_resurrect_stale_entries() {
        let temp = tempfile::tempdir().unwrap();
        let project_root = temp.path();
        let file = project_root.join("src/lib.rs");
        write_source(&file, "pub fn alpha() -> i32 {\n    1\n}\n");

        let mut worker_index = build_recorded_test_index(project_root, std::slice::from_ref(&file));
        let mut serving_index = worker_index.clone();
        fs::write(&file, [0xff, 0xfe, 0xfd]).unwrap();

        let mut embedder = RecordingEmbedder::default();
        let mut embed = |texts: Vec<String>| embedder.embed(texts);
        let mut progress = |_done: usize, _total: usize| {};
        let update = worker_index
            .refresh_invalidated_files(
                project_root,
                std::slice::from_ref(&file),
                &mut embed,
                16,
                100,
                &mut progress,
            )
            .unwrap();

        assert_eq!(embedder.total_embedded_texts(), 0);
        assert!(update.added_entries.is_empty());
        assert!(worker_index.entries.is_empty());
        assert!(!worker_index.file_mtimes.contains_key(&file));

        serving_index.apply_refresh_update(
            update.added_entries,
            update.updated_metadata,
            &update.completed_paths,
        );
        assert!(serving_index.entries.is_empty());
        assert!(!serving_index.file_mtimes.contains_key(&file));
    }

    #[test]
    fn refresh_invalidated_cap_deferral_remains_file_count_based() {
        let temp = tempfile::tempdir().unwrap();
        let project_root = temp.path();
        let indexed = project_root.join("src/a.rs");
        let deferred = project_root.join("src/b.rs");
        write_source(&indexed, "pub fn alpha() -> i32 {\n    1\n}\n");
        write_source(&deferred, "pub fn beta() -> i32 {\n    2\n}\n");

        let mut index = build_recorded_test_index(project_root, std::slice::from_ref(&indexed));
        let mut embedder = RecordingEmbedder::default();
        let mut embed = |texts: Vec<String>| embedder.embed(texts);
        let mut progress = |_done: usize, _total: usize| {};
        let update = index
            .refresh_invalidated_files(
                project_root,
                std::slice::from_ref(&deferred),
                &mut embed,
                16,
                1,
                &mut progress,
            )
            .unwrap();

        assert_eq!(update.summary.total_processed, 1);
        assert_eq!(update.summary.added, 0);
        assert_eq!(embedder.total_embedded_texts(), 0);
        assert_eq!(index.indexed_file_count(), 1);
        assert!(index.deferred_files.contains(&deferred));
        assert!(entries_for_file(&index, &deferred).is_empty());
    }

    #[test]
    fn semantic_cache_serialization_skips_paths_outside_project_root() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = fs::canonicalize(dir.path()).expect("canonical project");
        let outside = project.join("..").join("outside.rs");
        let mut index = SemanticIndex::new(project.clone(), 3);
        index
            .file_mtimes
            .insert(outside.clone(), SystemTime::UNIX_EPOCH);
        index.file_sizes.insert(outside.clone(), 1);
        index
            .file_hashes
            .insert(outside.clone(), cache_freshness::zero_hash());
        index.entries.push(EmbeddingEntry {
            chunk: SemanticChunk {
                file: outside,
                name: "outside".to_string(),
                qualified_name: None,
                kind: SymbolKind::Function,
                start_line: 0,
                end_line: 0,
                exported: false,
                embed_text: "outside".to_string(),
                snippet: "outside".to_string(),
            },
            norm: vector_norm(&[1.0, 0.0, 0.0]),
            vector: vec![1.0, 0.0, 0.0],
        });

        let bytes = index.to_bytes();
        let loaded = SemanticIndex::from_bytes(&bytes, &project).expect("load serialized index");
        assert_eq!(loaded.entries.len(), 0);
        assert!(loaded.file_mtimes.is_empty());
    }

    #[test]
    fn semantic_search_bounded_top_k_matches_reference_full_sort() {
        let project_root = test_project_root();
        let file = project_root.join("src/lib.rs");
        let mut index = SemanticIndex::new(project_root, 2);
        let entries = [
            ("alpha", vec![2.0, 0.0], false),
            ("beta", vec![0.0, 3.0], false),
            ("gamma", vec![4.0, 0.0], false),
            ("delta", vec![1.0, 1.0], true),
            ("epsilon", vec![-5.0, 0.0], false),
        ];
        for (line, (name, vector, exported)) in entries.into_iter().enumerate() {
            index.entries.push(EmbeddingEntry {
                chunk: SemanticChunk {
                    file: file.clone(),
                    name: name.to_string(),
                    qualified_name: None,
                    kind: SymbolKind::Function,
                    start_line: line as u32 + 1,
                    end_line: line as u32 + 1,
                    exported,
                    embed_text: name.to_string(),
                    snippet: format!("fn {name}() {{}}"),
                },
                norm: vector_norm(&vector),
                vector,
            });
        }

        let query = vec![2.0, 0.0];
        let top_k = 4;
        let mut reference: Vec<(f32, usize)> = index
            .entries
            .iter()
            .enumerate()
            .map(|(idx, entry)| {
                // Recompute both norms for every entry as the reference
                // implementation, so cached norms cannot change ranking or scores.
                let mut dot = 0.0f32;
                let mut query_squared_norm = 0.0f32;
                let mut entry_squared_norm = 0.0f32;
                for i in 0..query.len() {
                    dot += query[i] * entry.vector[i];
                    query_squared_norm += query[i] * query[i];
                    entry_squared_norm += entry.vector[i] * entry.vector[i];
                }
                let denom = query_squared_norm.sqrt() * entry_squared_norm.sqrt();
                let mut score = if denom == 0.0 { 0.0 } else { dot / denom };
                if entry.chunk.exported {
                    score *= 1.1;
                }
                (score, idx)
            })
            .collect();
        reference.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        let expected: Vec<(String, f32)> = reference
            .into_iter()
            .take(top_k)
            .map(|(score, idx)| (index.entries[idx].chunk.name.clone(), score))
            .collect();

        let actual: Vec<(String, f32)> = index
            .search(&query, top_k)
            .into_iter()
            .map(|result| (result.name, result.score))
            .collect();

        assert_eq!(
            actual.iter().map(|(name, _)| name).collect::<Vec<_>>(),
            expected.iter().map(|(name, _)| name).collect::<Vec<_>>()
        );
        for ((_, actual_score), (_, expected_score)) in actual.iter().zip(expected.iter()) {
            assert!((actual_score - expected_score).abs() < 1e-6);
        }
        assert_eq!(actual[0].0, "alpha");
        assert_eq!(actual[1].0, "gamma", "equal scores keep insertion order");
        assert!(index.search(&query, 0).is_empty());
    }

    #[test]
    fn test_cosine_similarity_identical() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![1.0, 0.0, 0.0];
        assert!((cosine_similarity(&a, &b) - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_cosine_similarity_orthogonal() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![0.0, 1.0, 0.0];
        assert!(cosine_similarity(&a, &b).abs() < 0.001);
    }

    #[test]
    fn test_cosine_similarity_opposite() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![-1.0, 0.0, 0.0];
        assert!((cosine_similarity(&a, &b) + 1.0).abs() < 0.001);
    }

    #[test]
    fn test_serialization_roundtrip() {
        let project_root = test_project_root();
        let file = project_root.join("src/main.rs");
        let mut index = SemanticIndex::new(project_root.clone(), DEFAULT_DIMENSION);
        index.entries.push(EmbeddingEntry {
            chunk: SemanticChunk {
                file: file.clone(),
                name: "handle_request".to_string(),
                qualified_name: None,
                kind: SymbolKind::Function,
                start_line: 10,
                end_line: 25,
                exported: true,
                embed_text: "file:src/main.rs kind:function name:handle_request".to_string(),
                snippet: "fn handle_request() {\n  // ...\n}".to_string(),
            },
            norm: vector_norm(&[0.1, 0.2, 0.3, 0.4]),
            vector: vec![0.1, 0.2, 0.3, 0.4],
        });
        index.dimension = 4;
        index
            .file_mtimes
            .insert(file.clone(), SystemTime::UNIX_EPOCH);
        index.file_sizes.insert(file, 0);
        index.set_fingerprint(SemanticIndexFingerprint {
            backend: "fastembed".to_string(),
            model: "all-MiniLM-L6-v2".to_string(),
            base_url: FALLBACK_BACKEND.to_string(),
            dimension: 4,
            chunking_version: default_chunking_version(),
            ..Default::default()
        });

        let bytes = index.to_bytes();
        let restored = SemanticIndex::from_bytes(&bytes, &project_root).unwrap();

        assert_eq!(restored.entries.len(), 1);
        assert_eq!(restored.entries[0].chunk.name, "handle_request");
        assert_eq!(restored.entries[0].vector, vec![0.1, 0.2, 0.3, 0.4]);
        assert_eq!(
            restored.entries[0].norm,
            vector_norm(&restored.entries[0].vector)
        );
        assert_eq!(restored.dimension, 4);
        assert_eq!(restored.backend_label(), Some("fastembed"));
        assert_eq!(restored.model_label(), Some("all-MiniLM-L6-v2"));
    }

    #[test]
    fn semantic_cache_v6_loads_and_v7_round_trips_qualified_names() {
        let storage = tempfile::tempdir().expect("create storage dir");
        let project = storage.path().join("project");
        fs::create_dir_all(project.join("src")).expect("create project src");
        let file = project.join("src/lib.rs");
        fs::write(&file, "pub fn alpha() {}\npub fn beta() {}\n").expect("write source");
        let project_root = fs::canonicalize(&project).expect("canonical project");
        let file = fs::canonicalize(&file).expect("canonical file");

        let mut index = SemanticIndex::new(project_root.clone(), 3);
        let mtime = SystemTime::UNIX_EPOCH + Duration::new(123, 456);
        index.file_mtimes.insert(file.clone(), mtime);
        index.file_sizes.insert(file.clone(), 42);
        index
            .file_hashes
            .insert(file.clone(), cache_freshness::zero_hash());
        index.entries.push(EmbeddingEntry {
            chunk: SemanticChunk {
                file: file.clone(),
                name: "alpha".to_string(),
                qualified_name: Some("Service.alpha".to_string()),
                kind: SymbolKind::Function,
                start_line: 0,
                end_line: 0,
                exported: true,
                embed_text: "file:src/lib.rs kind:function name:alpha".to_string(),
                snippet: "pub fn alpha() {}".to_string(),
            },
            norm: vector_norm(&[0.1, 0.2, 0.3]),
            vector: vec![0.1, 0.2, 0.3],
        });
        index.entries.push(EmbeddingEntry {
            chunk: SemanticChunk {
                file: file.clone(),
                name: "beta".to_string(),
                qualified_name: Some("Service.beta".to_string()),
                kind: SymbolKind::Function,
                start_line: 1,
                end_line: 1,
                exported: true,
                embed_text: "file:src/lib.rs kind:function name:beta".to_string(),
                snippet: "pub fn beta() {}".to_string(),
            },
            norm: vector_norm(&[0.4, 0.5, 0.6]),
            vector: vec![0.4, 0.5, 0.6],
        });
        let fingerprint = SemanticIndexFingerprint {
            backend: "fastembed".to_string(),
            model: "all-MiniLM-L6-v2".to_string(),
            base_url: FALLBACK_BACKEND.to_string(),
            dimension: 3,
            chunking_version: default_chunking_version(),
            ..Default::default()
        };
        let fingerprint_before = fingerprint.as_string();
        index.set_fingerprint(fingerprint.clone());

        let legacy_bytes = legacy_semantic_index_bytes(&index);
        assert_eq!(legacy_bytes[0], SEMANTIC_INDEX_VERSION_V6);
        let legacy_dir = storage.path().join("semantic/legacy-proj");
        fs::create_dir_all(&legacy_dir).expect("create legacy semantic dir");
        let legacy_path = legacy_dir.join("semantic.bin");
        fs::write(&legacy_path, &legacy_bytes).expect("write legacy semantic.bin");
        let legacy_loaded = SemanticIndex::read_from_disk(
            storage.path(),
            "legacy-proj",
            &project_root,
            false,
            Some(&fingerprint_before),
        )
        .expect("load v6 semantic index");
        assert!(
            legacy_path.exists(),
            "compatible V6 cache must not be deleted"
        );
        assert!(legacy_loaded
            .entries
            .iter()
            .all(|entry| entry.chunk.qualified_name.is_none()));
        assert_eq!(
            legacy_loaded.fingerprint().unwrap().as_string(),
            fingerprint_before
        );

        let v7_bytes = index.to_bytes();
        assert_eq!(v7_bytes[0], SEMANTIC_INDEX_VERSION_V7);
        assert_ne!(v7_bytes, legacy_bytes);
        let restored = SemanticIndex::from_bytes(&v7_bytes, &project_root).unwrap();
        assert_eq!(
            restored.entries[0].chunk.qualified_name.as_deref(),
            Some("Service.alpha")
        );
        assert_eq!(
            restored.entries[1].chunk.qualified_name.as_deref(),
            Some("Service.beta")
        );
        assert_eq!(
            restored.fingerprint().unwrap().as_string(),
            fingerprint_before
        );

        index.write_to_disk(storage.path(), "proj");
        let data_path = storage.path().join("semantic/proj/semantic.bin");
        let persisted = fs::read(&data_path).expect("read semantic.bin");
        assert_eq!(persisted[0], SEMANTIC_INDEX_VERSION_V7);

        let loaded = SemanticIndex::read_from_disk(
            storage.path(),
            "proj",
            &project_root,
            false,
            Some(&fingerprint_before),
        )
        .expect("load semantic index");
        assert_eq!(loaded.entries.len(), index.entries.len());
        assert_eq!(loaded.dimension, index.dimension);
        assert_eq!(
            loaded.fingerprint().unwrap().as_string(),
            fingerprint_before
        );
        assert_eq!(loaded.file_mtimes.get(&file), Some(&mtime));
        assert_eq!(loaded.file_sizes.get(&file), Some(&42));
        assert_eq!(
            loaded.file_hashes.get(&file),
            Some(&cache_freshness::zero_hash())
        );
        for (actual, expected) in loaded.entries.iter().zip(index.entries.iter()) {
            assert_eq!(actual.chunk.file, expected.chunk.file);
            assert_eq!(actual.chunk.name, expected.chunk.name);
            assert_eq!(actual.chunk.qualified_name, expected.chunk.qualified_name);
            assert_eq!(actual.chunk.kind, expected.chunk.kind);
            assert_eq!(actual.chunk.start_line, expected.chunk.start_line);
            assert_eq!(actual.chunk.end_line, expected.chunk.end_line);
            assert_eq!(actual.chunk.exported, expected.chunk.exported);
            assert_eq!(actual.chunk.embed_text, expected.chunk.embed_text);
            assert_eq!(actual.chunk.snippet, expected.chunk.snippet);
            assert_eq!(actual.vector, expected.vector);
        }
        assert_eq!(loaded.to_bytes(), persisted);
        assert_eq!(fingerprint.as_string(), fingerprint_before);
    }

    #[test]
    fn symbol_kind_serialization_roundtrip_includes_file_summary_variant() {
        let cases = [
            (SymbolKind::Function, 0),
            (SymbolKind::Class, 1),
            (SymbolKind::Method, 2),
            (SymbolKind::Struct, 3),
            (SymbolKind::Interface, 4),
            (SymbolKind::Enum, 5),
            (SymbolKind::TypeAlias, 6),
            (SymbolKind::Variable, 7),
            (SymbolKind::Heading, 8),
            (SymbolKind::FileSummary, 9),
        ];

        for (kind, encoded) in cases {
            assert_eq!(symbol_kind_to_u8(&kind), encoded);
            assert_eq!(u8_to_symbol_kind(encoded), kind);
        }
    }

    #[test]
    fn test_search_top_k() {
        let mut index = SemanticIndex::new(test_project_root(), DEFAULT_DIMENSION);
        index.dimension = 3;

        // Add entries with known vectors
        for (i, name) in ["auth", "database", "handler"].iter().enumerate() {
            let mut vec = vec![0.0f32; 3];
            vec[i] = 1.0; // orthogonal vectors
            index.entries.push(EmbeddingEntry {
                chunk: SemanticChunk {
                    file: PathBuf::from("/src/lib.rs"),
                    name: name.to_string(),
                    qualified_name: None,
                    kind: SymbolKind::Function,
                    start_line: (i * 10 + 1) as u32,
                    end_line: (i * 10 + 5) as u32,
                    exported: true,
                    embed_text: format!("kind:function name:{}", name),
                    snippet: format!("fn {}() {{}}", name),
                },
                norm: vector_norm(&vec),
                vector: vec,
            });
        }

        // Query aligned with "auth" (index 0)
        let query = vec![0.9, 0.1, 0.0];
        let results = index.search(&query, 2);

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].name, "auth"); // highest score
        assert!(results[0].score > results[1].score);
    }

    #[test]
    fn test_empty_index_search() {
        let index = SemanticIndex::new(test_project_root(), DEFAULT_DIMENSION);
        let results = index.search(&[0.1, 0.2, 0.3], 10);
        assert!(results.is_empty());
    }

    #[test]
    fn single_line_symbol_builds_non_empty_snippet() {
        let symbol = Symbol {
            name: "answer".to_string(),
            kind: SymbolKind::Variable,
            range: crate::symbols::Range {
                start_line: 0,
                start_col: 0,
                end_line: 0,
                end_col: 24,
            },
            signature: Some("const answer = 42".to_string()),
            scope_chain: Vec::new(),
            exported: true,
            parent: None,
        };
        let source = "export const answer = 42;\n";

        let snippet = build_snippet(&symbol, source);

        assert_eq!(snippet, "export const answer = 42;");
    }

    #[test]
    fn metal_chunk_collection_uses_shader_function_boundaries() {
        let project_root = Path::new("/project");
        let file = project_root.join("sample.metal");
        let source = include_str!("../tests/fixtures/sample.metal");
        let chunks = collect_file_chunks_from_source(
            project_root,
            &file,
            crate::parser::LangId::Metal,
            source,
        )
        .expect("collect Metal chunks");

        let helper = chunks
            .iter()
            .find(|chunk| chunk.name == "brighten")
            .expect("helper chunk");
        assert_eq!((helper.start_line, helper.end_line), (3, 5));
        assert!(!helper.snippet.contains("brighten_buffer"));

        let shader = chunks
            .iter()
            .find(|chunk| chunk.name == "brighten_buffer")
            .expect("shader chunk");
        assert_eq!(shader.kind, SymbolKind::Function);
        assert_eq!((shader.start_line, shader.end_line), (7, 9));
        assert!(shader.snippet.starts_with("kernel void brighten_buffer"));
        assert!(shader.snippet.contains("brighten(values[id])"));
    }

    #[test]
    fn cuda_chunk_collection_uses_function_boundaries() {
        let project_root = Path::new("/project");
        let file = project_root.join("sample.cu");
        let source = include_str!("../tests/fixtures/sample.cu");
        let chunks = collect_file_chunks_from_source(
            project_root,
            &file,
            crate::parser::LangId::Cuda,
            source,
        )
        .expect("collect CUDA chunks");

        let kernel = chunks
            .iter()
            .find(|chunk| chunk.name == "transform")
            .expect("kernel chunk");
        assert_eq!(kernel.kind, SymbolKind::Kernel);
        assert_eq!((kernel.start_line, kernel.end_line), (4, 7));
        assert!(kernel.snippet.contains("scale(data[index])"));
        assert!(!kernel.snippet.contains("launch_transform"));

        let host = chunks
            .iter()
            .find(|chunk| chunk.name == "launch_transform")
            .expect("host function chunk");
        assert_eq!((host.start_line, host.end_line), (9, 11));
        assert!(host.snippet.contains("transform<<<grid, block>>>(data)"));
    }

    #[test]
    fn toml_chunk_collection_uses_table_and_key_boundaries() {
        let project_root = Path::new("/project");
        let file = project_root.join("Cargo.toml");
        let source = "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n\n[dependencies.foo]\nversion = \"1\"\n";
        let chunks = collect_file_chunks_from_source(
            project_root,
            &file,
            crate::parser::LangId::Toml,
            source,
        )
        .expect("collect TOML chunks");

        let package = chunks
            .iter()
            .find(|chunk| chunk.name == "package")
            .expect("package table chunk");
        assert_eq!((package.start_line, package.end_line), (0, 2));
        assert!(package.snippet.contains("name = \"demo\""));
        assert!(!package.snippet.contains("dependencies.foo"));

        let name = chunks
            .iter()
            .find(|chunk| chunk.qualified_name.as_deref() == Some("package.name"))
            .expect("nested package.name key chunk");
        assert_eq!((name.start_line, name.end_line), (1, 1));
        assert_eq!(name.snippet, "name = \"demo\"");

        let dependency = chunks
            .iter()
            .find(|chunk| chunk.name == "dependencies.foo")
            .expect("dependency table chunk");
        assert_eq!((dependency.start_line, dependency.end_line), (4, 5));
    }

    #[test]
    fn optimized_file_chunk_collection_matches_file_parser_path() {
        let project_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let file = project_root.join("src/semantic_index.rs");
        let source = std::fs::read_to_string(&file).unwrap();

        let mut legacy_parser = FileParser::new();
        let legacy_symbols = legacy_parser.extract_symbols(&file).unwrap();
        let legacy_chunks = symbols_to_chunks(&file, &legacy_symbols, &source, &project_root);

        let optimized_chunks = collect_file_chunks(&project_root, &file).unwrap();

        assert_eq!(
            chunk_fingerprint(&optimized_chunks),
            chunk_fingerprint(&legacy_chunks)
        );
    }

    #[test]
    fn collect_file_chunks_indexes_java_symbols() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("Greeter.java");
        std::fs::write(
            &file,
            r#"package example;

public class Greeter {
    public String greet(String name) {
        return "Hello, " + name;
    }
}
"#,
        )
        .unwrap();

        let chunks = collect_file_chunks(dir.path(), &file).unwrap();

        assert!(
            !chunks.is_empty(),
            "Java file should produce semantic chunks"
        );
        assert!(
            chunks
                .iter()
                .any(|chunk| chunk.name == "Greeter" && chunk.kind == SymbolKind::Class),
            "Java class symbol should be chunked: {chunks:?}"
        );
        assert!(
            chunks
                .iter()
                .any(|chunk| chunk.name == "greet" && chunk.kind == SymbolKind::Method),
            "Java method symbol should be chunked: {chunks:?}"
        );
    }

    fn chunk_fingerprint(
        chunks: &[SemanticChunk],
    ) -> Vec<(String, SymbolKind, u32, u32, bool, String, String)> {
        chunks
            .iter()
            .map(|chunk| {
                (
                    chunk.name.clone(),
                    chunk.kind.clone(),
                    chunk.start_line,
                    chunk.end_line,
                    chunk.exported,
                    chunk.embed_text.clone(),
                    chunk.snippet.clone(),
                )
            })
            .collect()
    }

    #[test]
    fn collect_file_chunks_skips_oversized_file() {
        let dir = tempfile::tempdir().unwrap();
        let big = dir.path().join("huge.ts");
        // Just over the cap: a valid TS file that would otherwise yield chunks.
        let filler = "export const x = 1;\n"
            .repeat(((MAX_SEMANTIC_FILE_BYTES as usize) / "export const x = 1;\n".len()) + 16);
        std::fs::write(&big, &filler).unwrap();
        assert!(big.metadata().unwrap().len() > MAX_SEMANTIC_FILE_BYTES);

        // Oversized → tracked with zero chunks, NOT an error (so the caller keeps
        // the file in metadata and freshness skips re-reading it).
        let chunks = collect_file_chunks(dir.path(), &big).unwrap();
        assert!(chunks.is_empty(), "oversized file must yield no chunks");

        // A small file of the same language still produces chunks.
        let small = dir.path().join("small.ts");
        std::fs::write(&small, "export function foo() { return 1; }\n").unwrap();
        let small_chunks = collect_file_chunks(dir.path(), &small).unwrap();
        assert!(!small_chunks.is_empty(), "small file should still chunk");
    }

    #[test]
    fn rejects_oversized_dimension_during_deserialization() {
        let mut bytes = Vec::new();
        bytes.push(1u8);
        bytes.extend_from_slice(&((MAX_DIMENSION as u32) + 1).to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());

        assert!(SemanticIndex::from_bytes(&bytes, &test_project_root()).is_err());
    }

    #[test]
    fn rejects_oversized_entry_count_during_deserialization() {
        let mut bytes = Vec::new();
        bytes.push(1u8);
        bytes.extend_from_slice(&(DEFAULT_DIMENSION as u32).to_le_bytes());
        bytes.extend_from_slice(&((MAX_ENTRIES as u32) + 1).to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());

        assert!(SemanticIndex::from_bytes(&bytes, &test_project_root()).is_err());
    }

    fn add_invalidation_fixture_entry(index: &mut SemanticIndex, file: PathBuf, ordinal: u64) {
        index.entries.push(EmbeddingEntry::new(
            SemanticChunk {
                file: file.clone(),
                name: format!("symbol_{ordinal}"),
                qualified_name: None,
                kind: SymbolKind::Function,
                start_line: ordinal as u32,
                end_line: ordinal as u32 + 1,
                exported: false,
                embed_text: format!("symbol {ordinal}"),
                snippet: format!("fn symbol_{ordinal}() {{}}"),
            },
            vec![ordinal as f32 + 1.0, 1.0],
        ));
        let mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(ordinal + 1);
        index.file_mtimes.insert(file.clone(), mtime);
        index.file_sizes.insert(file.clone(), ordinal + 10);
        index
            .file_hashes
            .insert(file, blake3::hash(&ordinal.to_le_bytes()));
    }

    #[test]
    fn freezing_declines_instead_of_panicking_when_a_dirty_path_is_outside_the_root() {
        // A delta path outside the root once turned the freeze into a panic:
        // the shareability check covered entries, metadata maps and deferred
        // files but not the dirty-path set, and the move then hit an expect.
        // Under the daemon that panic is a fatal actor exit (exit 4 three
        // times on 2026-09-14).
        let temp = tempfile::tempdir().unwrap();
        let project_root = temp
            .path()
            .join("owner")
            .canonicalize()
            .unwrap_or_else(|_| {
                fs::create_dir_all(temp.path().join("owner")).unwrap();
                temp.path().join("owner").canonicalize().unwrap()
            });
        let borrower = temp.path().join("borrower");
        fs::create_dir_all(&borrower).unwrap();
        let mut index = SemanticIndex::new(project_root.clone(), 2);
        let file = project_root.join("file_0.rs");
        fs::write(&file, "fn symbol_0() {}\n").unwrap();
        add_invalidation_fixture_entry(&mut index, file, 0);
        let config = SemanticBackendConfig::default();
        index.set_fingerprint(SemanticIndexFingerprint::for_config_dimension(&config, 2));
        // A fresh index carries no delta set (None means "structural diff");
        // seed one the way a refresh does so the stray path is really carried.
        index.set_dirty_paths(Some(BTreeSet::from([temp
            .path()
            .join("elsewhere")
            .join("stray.rs")])));
        let entries_before = index.entries.len();

        let adopted = index.adopt_frozen_base_for_root(&borrower, &config);

        assert!(adopted.is_none(), "an unshareable index must stay private");
        assert!(index.shared_base.is_none());
        assert_eq!(
            index.entries.len(),
            entries_before,
            "the private index survives intact"
        );

        // The same index with an in-root dirty path freezes normally.
        let mut shareable = SemanticIndex::new(project_root.clone(), 2);
        let file = project_root.join("file_1.rs");
        fs::write(&file, "fn symbol_1() {}\n").unwrap();
        add_invalidation_fixture_entry(&mut shareable, file.clone(), 1);
        shareable.set_fingerprint(SemanticIndexFingerprint::for_config_dimension(&config, 2));
        shareable.set_dirty_paths(Some(BTreeSet::from([file])));
        assert!(shareable
            .adopt_frozen_base_for_root(&borrower, &config)
            .is_some());
        assert!(shareable.shared_base.is_some());
    }

    #[test]
    fn batch_invalidation_matches_sequential_calls_with_one_retain_pass() {
        let temp = tempfile::tempdir().unwrap();
        let project_root = temp.path().canonicalize().unwrap();
        let mut source = SemanticIndex::new(project_root.clone(), 2);
        let files = (0..8)
            .map(|ordinal| {
                let file = project_root.join(format!("file_{ordinal}.rs"));
                fs::write(&file, format!("fn symbol_{ordinal}() {{}}\n")).unwrap();
                add_invalidation_fixture_entry(&mut source, file.clone(), ordinal);
                file
            })
            .collect::<Vec<_>>();
        let invalidated = vec![files[1].clone(), files[3].clone(), files[6].clone()];

        let shared = Arc::new(source.into_shared_base().ok().unwrap());
        let mut shared_batched =
            SemanticIndex::from_shared_base(project_root.clone(), Arc::clone(&shared));
        shared_batched.invalidate_files(&invalidated);
        let mut source = SemanticIndex::from_shared_base(project_root, shared);
        source.materialize_shared_base();
        let mut sequential = source.clone();
        let mut batched = source;
        for file in &invalidated {
            sequential.invalidate_file(file);
        }
        batched.invalidate_files(&invalidated);

        assert!(sequential.shared_base.is_none());
        assert!(batched.shared_base.is_none());
        assert!(shared_batched.shared_base.is_none());
        assert_eq!(batched.to_bytes(), sequential.to_bytes());
        assert_eq!(shared_batched.file_mtimes, batched.file_mtimes);
        assert_eq!(shared_batched.file_sizes, batched.file_sizes);
        assert_eq!(shared_batched.file_hashes, batched.file_hashes);
        assert_eq!(
            format!("{:?}", shared_batched.entries),
            format!("{:?}", batched.entries)
        );
        assert_eq!(
            sequential.removal_retain_passes_for_test(),
            invalidated.len()
        );
        assert_eq!(batched.removal_retain_passes_for_test(), 1);
        assert_eq!(shared_batched.removal_retain_passes_for_test(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn batch_invalidation_removes_raw_and_canonical_alias_metadata() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let project_root = temp.path().canonicalize().unwrap();
        let real_dir = project_root.join("real");
        let alias_dir = project_root.join("alias");
        fs::create_dir(&real_dir).unwrap();
        symlink(&real_dir, &alias_dir).unwrap();
        let real_file = real_dir.join("lib.rs");
        let alias_file = alias_dir.join("lib.rs");
        let untouched = project_root.join("untouched.rs");
        fs::write(&real_file, "fn aliased() {}\n").unwrap();
        fs::write(&untouched, "fn untouched() {}\n").unwrap();
        assert_eq!(fs::canonicalize(&alias_file).unwrap(), real_file);

        let mut index = SemanticIndex::new(project_root, 2);
        add_invalidation_fixture_entry(&mut index, alias_file.clone(), 1);
        add_invalidation_fixture_entry(&mut index, real_file.clone(), 2);
        add_invalidation_fixture_entry(&mut index, untouched.clone(), 3);
        let mut sequential = index.clone();
        sequential.invalidate_file(&alias_file);
        index.invalidate_files(std::slice::from_ref(&alias_file));

        assert_eq!(index.to_bytes(), sequential.to_bytes());
        assert!(index
            .entries
            .iter()
            .all(|entry| entry.chunk.file != alias_file && entry.chunk.file != real_file));
        assert!(!index.file_mtimes.contains_key(&alias_file));
        assert!(!index.file_mtimes.contains_key(&real_file));
        assert!(index.file_mtimes.contains_key(&untouched));
        assert!(!index.file_sizes.contains_key(&alias_file));
        assert!(!index.file_sizes.contains_key(&real_file));
        assert!(index.file_sizes.contains_key(&untouched));
        assert!(!index.file_hashes.contains_key(&alias_file));
        assert!(!index.file_hashes.contains_key(&real_file));
        assert!(index.file_hashes.contains_key(&untouched));
        assert_eq!(index.removal_retain_passes_for_test(), 1);
    }

    #[test]
    fn invalidate_file_removes_entries_and_mtime() {
        let target = PathBuf::from("/src/main.rs");
        let mut index = SemanticIndex::new(test_project_root(), DEFAULT_DIMENSION);
        index.entries.push(EmbeddingEntry {
            chunk: SemanticChunk {
                file: target.clone(),
                name: "main".to_string(),
                qualified_name: None,
                kind: SymbolKind::Function,
                start_line: 0,
                end_line: 1,
                exported: false,
                embed_text: "main".to_string(),
                snippet: "fn main() {}".to_string(),
            },
            norm: vector_norm(&[1.0; DEFAULT_DIMENSION]),
            vector: vec![1.0; DEFAULT_DIMENSION],
        });
        index
            .file_mtimes
            .insert(target.clone(), SystemTime::UNIX_EPOCH);
        index.file_sizes.insert(target.clone(), 0);

        index.invalidate_file(&target);

        assert!(index.entries.is_empty());
        assert!(!index.file_mtimes.contains_key(&target));
        assert!(!index.file_sizes.contains_key(&target));
    }

    #[test]
    fn refresh_missing_changed_file_is_purged_after_collect() {
        let temp = tempfile::tempdir().unwrap();
        let project_root = temp.path();
        let file = project_root.join("src/lib.rs");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        write_rust_file(&file, "vanished_symbol");

        let mut index = build_test_index(project_root, std::slice::from_ref(&file));
        let original_size = *index.file_sizes.get(&file).unwrap();
        set_file_metadata(&mut index, &file, SystemTime::UNIX_EPOCH, original_size + 1);
        fs::remove_file(&file).unwrap();

        let mut embed = test_vector_for_texts;
        let mut progress = |_done: usize, _total: usize| {};
        let summary = index
            .refresh_stale_files(
                project_root,
                std::slice::from_ref(&file),
                &mut embed,
                8,
                &mut progress,
            )
            .unwrap();

        assert_eq!(summary.changed, 0);
        assert_eq!(summary.added, 0);
        assert_eq!(summary.deleted, 1);
        assert!(index.entries.is_empty());
        assert!(!index.file_mtimes.contains_key(&file));
        assert!(!index.file_sizes.contains_key(&file));
        assert!(!index.file_hashes.contains_key(&file));
    }

    #[test]
    fn refresh_collect_error_for_existing_path_preserves_cached_entry() {
        let temp = tempfile::tempdir().unwrap();
        let project_root = temp.path();
        let file = project_root.join("src/lib.rs");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        write_rust_file(&file, "kept_symbol");

        let mut index = build_test_index(project_root, std::slice::from_ref(&file));
        let original_entry_count = index.entries.len();
        let original_mtime = *index.file_mtimes.get(&file).unwrap();
        let original_size = *index.file_sizes.get(&file).unwrap();

        let stale_mtime = SystemTime::UNIX_EPOCH;
        set_file_metadata(&mut index, &file, stale_mtime, original_size + 1);
        fs::remove_file(&file).unwrap();
        fs::create_dir(&file).unwrap();

        let mut embed = test_vector_for_texts;
        let mut progress = |_done: usize, _total: usize| {};
        let summary = index
            .refresh_stale_files(
                project_root,
                std::slice::from_ref(&file),
                &mut embed,
                8,
                &mut progress,
            )
            .unwrap();

        assert_eq!(summary.changed, 0);
        assert_eq!(summary.added, 0);
        assert_eq!(summary.deleted, 0);
        assert_eq!(index.entries.len(), original_entry_count);
        assert!(index
            .entries
            .iter()
            .any(|entry| entry.chunk.name == "kept_symbol"));
        assert_eq!(index.file_mtimes.get(&file), Some(&stale_mtime));
        assert_ne!(index.file_mtimes.get(&file), Some(&original_mtime));
        assert_eq!(index.file_sizes.get(&file), Some(&(original_size + 1)));
    }

    #[test]
    fn refresh_never_indexed_file_error_does_not_record_mtime() {
        let temp = tempfile::tempdir().unwrap();
        let project_root = temp.path();
        let missing = project_root.join("src/missing.rs");
        fs::create_dir_all(missing.parent().unwrap()).unwrap();

        let mut index = SemanticIndex::new(test_project_root(), DEFAULT_DIMENSION);
        let mut embed = test_vector_for_texts;
        let mut progress = |_done: usize, _total: usize| {};
        let summary = index
            .refresh_stale_files(
                project_root,
                std::slice::from_ref(&missing),
                &mut embed,
                8,
                &mut progress,
            )
            .unwrap();

        assert_eq!(summary.added, 0);
        assert_eq!(summary.changed, 0);
        assert_eq!(summary.deleted, 0);
        assert!(!index.file_mtimes.contains_key(&missing));
        assert!(!index.file_sizes.contains_key(&missing));
        assert!(index.entries.is_empty());
    }

    #[test]
    fn refresh_reports_added_for_new_files() {
        let temp = tempfile::tempdir().unwrap();
        let project_root = temp.path();
        let existing = project_root.join("src/lib.rs");
        let added = project_root.join("src/new.rs");
        fs::create_dir_all(existing.parent().unwrap()).unwrap();
        write_rust_file(&existing, "existing_symbol");
        write_rust_file(&added, "added_symbol");

        let mut index = build_test_index(project_root, std::slice::from_ref(&existing));
        let mut embed = test_vector_for_texts;
        let mut progress = |_done: usize, _total: usize| {};
        let summary = index
            .refresh_stale_files(
                project_root,
                &[existing.clone(), added.clone()],
                &mut embed,
                8,
                &mut progress,
            )
            .unwrap();

        assert_eq!(summary.added, 1);
        assert_eq!(summary.changed, 0);
        assert_eq!(summary.deleted, 0);
        assert_eq!(summary.total_processed, 2);
        assert!(index.file_mtimes.contains_key(&added));
        assert!(index.entries.iter().any(|entry| entry.chunk.file == added));
    }

    #[test]
    fn refresh_reports_deleted_for_removed_files() {
        let temp = tempfile::tempdir().unwrap();
        let project_root = temp.path();
        let deleted = project_root.join("src/deleted.rs");
        fs::create_dir_all(deleted.parent().unwrap()).unwrap();
        write_rust_file(&deleted, "deleted_symbol");

        let mut index = build_test_index(project_root, std::slice::from_ref(&deleted));
        fs::remove_file(&deleted).unwrap();

        let mut embed = test_vector_for_texts;
        let mut progress = |_done: usize, _total: usize| {};
        let summary = index
            .refresh_stale_files(project_root, &[], &mut embed, 8, &mut progress)
            .unwrap();

        assert_eq!(summary.deleted, 1);
        assert_eq!(summary.changed, 0);
        assert_eq!(summary.added, 0);
        assert_eq!(summary.total_processed, 1);
        assert!(!index.file_mtimes.contains_key(&deleted));
        assert!(index.entries.is_empty());
    }

    #[test]
    fn refresh_reports_changed_for_modified_files() {
        let temp = tempfile::tempdir().unwrap();
        let project_root = temp.path();
        let file = project_root.join("src/lib.rs");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        write_rust_file(&file, "old_symbol");

        let mut index = build_test_index(project_root, std::slice::from_ref(&file));
        set_file_metadata(&mut index, &file, SystemTime::UNIX_EPOCH, 0);
        write_rust_file(&file, "new_symbol");

        let mut embed = test_vector_for_texts;
        let mut progress = |_done: usize, _total: usize| {};
        let summary = index
            .refresh_stale_files(
                project_root,
                std::slice::from_ref(&file),
                &mut embed,
                8,
                &mut progress,
            )
            .unwrap();

        assert_eq!(summary.changed, 1);
        assert_eq!(summary.added, 0);
        assert_eq!(summary.deleted, 0);
        assert_eq!(summary.total_processed, 1);
        assert!(index
            .entries
            .iter()
            .any(|entry| entry.chunk.name == "new_symbol"));
        assert!(!index
            .entries
            .iter()
            .any(|entry| entry.chunk.name == "old_symbol"));
    }

    #[test]
    fn refresh_all_clean_reports_zero_counts_and_no_embedding_work() {
        let temp = tempfile::tempdir().unwrap();
        let project_root = temp.path();
        let file = project_root.join("src/lib.rs");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        write_rust_file(&file, "clean_symbol");

        let mut index = build_test_index(project_root, std::slice::from_ref(&file));
        let original_entries = index.entries.len();
        let mut embed_called = false;
        let mut embed = |texts: Vec<String>| {
            embed_called = true;
            test_vector_for_texts(texts)
        };
        let mut progress = |_done: usize, _total: usize| {};
        let summary = index
            .refresh_stale_files(
                project_root,
                std::slice::from_ref(&file),
                &mut embed,
                8,
                &mut progress,
            )
            .unwrap();

        assert!(summary.is_noop());
        assert_eq!(summary.total_processed, 1);
        assert!(!embed_called);
        assert_eq!(index.entries.len(), original_entries);
    }

    #[test]
    fn detects_missing_onnx_runtime_from_dynamic_load_error() {
        let message = "Failed to load ONNX Runtime shared library libonnxruntime.dylib via dlopen: no such file";

        assert!(is_onnx_runtime_unavailable(message));
    }

    #[test]
    fn formats_missing_onnx_runtime_with_install_hint() {
        let message = format_embedding_init_error(
            "Failed to load ONNX Runtime shared library libonnxruntime.so via dlopen: no such file",
        );

        assert!(message.starts_with(ONNX_RUNTIME_MISSING_PREFIX));
        assert!(message.contains("npx @cortexkit/aft doctor --fix"));
        assert!(message.contains("Original error:"));
    }

    /// The sidebar checks its rendering against this list, so the list has to
    /// stay a superset of what the daemon's own status producers say. The words
    /// below are produced here, by `status_label`; the rest come from the
    /// status, health, and search surfaces named on the constant.
    #[test]
    fn semantic_status_words_include_the_labels_this_module_produces() {
        let empty = SemanticIndex::new(PathBuf::from("/tmp/project"), 384);
        assert_eq!(empty.status_label(&SemanticIndexStatus::ready()), "empty");
        for label in ["disabled", "empty", "failed", "loading", "ready"] {
            assert!(
                SEMANTIC_INDEX_STATUS_WORDS.contains(&label),
                "{label} is emitted but not listed"
            );
        }

        let mut sorted = SEMANTIC_INDEX_STATUS_WORDS.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.as_slice(),
            SEMANTIC_INDEX_STATUS_WORDS,
            "keep the list sorted and duplicate-free; readers parse it as a set"
        );
    }

    /// `aft status` is the surface a user checks to answer "is semantic search
    /// working?". While this label was decided by the entry count alone it had
    /// no way to say no: a root whose index had failed reported `ready` there
    /// (or `empty`, for a failure that left nothing behind) while the sidebar
    /// and a search reply both reported the failure.
    #[test]
    fn failed_index_reports_the_failure_instead_of_its_entry_count() {
        let root = test_project_root();
        let mut populated = SemanticIndex::new(root.clone(), 3);
        populated.entries.push(EmbeddingEntry::new(
            SemanticChunk {
                file: root.join("lib.rs"),
                name: "indexed".to_string(),
                qualified_name: None,
                kind: SymbolKind::Function,
                start_line: 0,
                end_line: 1,
                exported: true,
                embed_text: "fn indexed".to_string(),
                snippet: "fn indexed() {}".to_string(),
            },
            vec![1.0, 0.0, 0.0],
        ));
        let empty = SemanticIndex::new(root, 3);
        let failed = SemanticIndexStatus::Failed("embedding backend died".to_string());

        for (index, entries) in [(&populated, "with entries"), (&empty, "without entries")] {
            assert_eq!(
                index.status_label(&failed),
                "failed",
                "a failed index {entries} must report the failure, never ready or empty"
            );
        }

        // The healthy words are unchanged: the entry count still separates an
        // index that can answer a query from one that holds nothing.
        assert_eq!(
            populated.status_label(&SemanticIndexStatus::ready()),
            "ready"
        );
        assert_eq!(empty.status_label(&SemanticIndexStatus::ready()), "empty");
        assert_eq!(
            populated.status_label(&SemanticIndexStatus::Disabled),
            "disabled"
        );
        assert_eq!(
            populated.status_label(&SemanticIndexStatus::Building {
                stage: "embed".to_string(),
                files: None,
                entries_done: None,
                entries_total: None,
            }),
            "loading",
            "a running build is not the leftover index object's readiness"
        );
    }

    /// The hint must not answer "can AFT download the runtime here?" itself.
    /// That question has one owner (`isOrtAutoDownloadSupported` in
    /// packages/aft-bridge/src/onnx-runtime.ts, asked by the CLI before it
    /// prints anything manual), and the daemon cannot reach it. The old hint
    /// guessed: it listed brew/apt/PATH instructions and then said the download
    /// was automatic, which is wrong advice on every platform AFT downloads for.
    #[test]
    fn onnx_install_hint_leaves_the_platform_question_to_its_one_owner() {
        let hint = ONNX_RUNTIME_INSTALL_HINT;

        for manual_advice in ["brew", "apt", "in your PATH", "onnxruntime.dll"] {
            assert!(
                !hint.contains(manual_advice),
                "hint names a platform-specific install route ({manual_advice}) it cannot know applies: {hint}"
            );
        }
        assert!(
            hint.contains("npx @cortexkit/aft doctor --fix"),
            "hint must name the command that installs the runtime, not just one that diagnoses: {hint}"
        );
        // Readers classify by this prefix; the hint is one of the messages they
        // classify, so it has to carry it.
        assert!(hint.starts_with(ONNX_RUNTIME_MISSING_PREFIX));
        assert!(is_onnx_runtime_unavailable(hint));
    }

    #[test]
    fn qwen_query_request_uses_documented_instruction_shape() {
        assert_eq!(
            query_embedding_text(
                "where is authentication handled",
                Some(crate::config::QWEN3_EMBEDDING_MODEL_CARD_RETRIEVAL_TASK),
            ),
            "Instruct: Given a web search query, retrieve relevant passages that answer the query\nQuery: where is authentication handled"
        );
        assert_eq!(
            query_embedding_text("where is authentication handled", None),
            "where is authentication handled"
        );
    }

    #[test]
    fn query_instruction_does_not_change_index_fingerprint() {
        let mut config = SemanticBackendConfig {
            backend: SemanticBackend::OpenAiCompatible,
            model: "text-embedding-qwen3-embedding-0.6b".to_string(),
            base_url: Some("http://127.0.0.1:1234/v1".to_string()),
            ..SemanticBackendConfig::default()
        };
        let automatic = SemanticIndexFingerprint::for_config_dimension(&config, 1024);
        config.query_instruction = "off".to_string();
        let off = SemanticIndexFingerprint::for_config_dimension(&config, 1024);
        config.query_instruction = crate::config::QWEN3_EMBEDDING_CODE_SEARCH_TASK.to_string();
        let literal = SemanticIndexFingerprint::for_config_dimension(&config, 1024);

        assert_eq!(automatic.as_string(), off.as_string());
        assert_eq!(off.as_string(), literal.as_string());
        assert!(automatic.matches(&off));
        assert!(off.matches(&literal));
    }

    #[test]
    fn query_embedding_cache_keys_the_text_sent_to_the_server() {
        let (base_url, inputs, handle) = start_recording_embedding_server(2);
        let config = SemanticBackendConfig {
            backend: SemanticBackend::OpenAiCompatible,
            model: "text-embedding-qwen3-embedding-0.6b".to_string(),
            base_url: Some(base_url),
            query_instruction: crate::config::QWEN3_EMBEDDING_CODE_SEARCH_TASK.to_string(),
            ..SemanticBackendConfig::default()
        };
        let budget = QueryBudget::from_config(&config);
        let mut model = SemanticEmbeddingModel::from_config(&config).unwrap();

        model.embed_query_cached("cache probe", budget).unwrap();
        model.query_instruction = None;
        model.embed_query_cached("cache probe", budget).unwrap();
        model.embed_query_cached("cache probe", budget).unwrap();
        handle.join().unwrap();

        assert_eq!(model.query_embedding_cache_stats(), (1, 2, 2));
        assert_eq!(
            *inputs.lock().unwrap(),
            vec![
                format!(
                    "Instruct: {}\nQuery: cache probe",
                    crate::config::QWEN3_EMBEDDING_CODE_SEARCH_TASK
                ),
                "cache probe".to_string(),
            ]
        );
    }

    #[test]
    fn interactive_query_budget_is_independent_from_build_timeout() {
        let mut config = SemanticBackendConfig {
            backend: SemanticBackend::OpenAiCompatible,
            model: "test-embedding".to_string(),
            base_url: Some("http://127.0.0.1:9".to_string()),
            api_key_env: None,
            timeout_ms: 0,
            query_timeout_ms: 0,
            max_batch_size: 64,
            max_files: 20_000,
            ..Default::default()
        };

        let build_model = SemanticEmbeddingModel::from_config(&config).unwrap();
        let query_model = SemanticEmbeddingModel::from_config_for_query(&config).unwrap();
        assert_eq!(
            build_model.timeout_ms(),
            DEFAULT_OPENAI_EMBEDDING_TIMEOUT_MS,
            "background build keeps the longer default embedding timeout"
        );
        assert_eq!(
            query_model.timeout_ms(),
            DEFAULT_OPENAI_EMBEDDING_TIMEOUT_MS,
            "a query-created model remains safe for later background build reuse"
        );
        assert_eq!(
            QueryBudget::from_config(&config).timeout_ms(),
            DEFAULT_SEMANTIC_QUERY_TIMEOUT_MS
        );

        config.timeout_ms = 60_000;
        assert_eq!(
            QueryBudget::from_config(&config).timeout_ms(),
            DEFAULT_SEMANTIC_QUERY_TIMEOUT_MS,
            "the build timeout must not affect interactive requests"
        );

        config.query_timeout_ms = 700;
        assert_eq!(QueryBudget::from_config(&config).timeout_ms(), 700);

        // A self-hosted embedding server on a slow box or a provider spike
        // (#320) is what the ceiling exists to admit; the transport still
        // bounds the whole search at 60 s, so 30 s leaves room for the
        // lexical fallback to render.
        config.query_timeout_ms = 30_000;
        assert_eq!(QueryBudget::from_config(&config).timeout_ms(), 30_000);
        config.query_timeout_ms = 45_000;
        assert_eq!(
            QueryBudget::from_config(&config).timeout_ms(),
            MAX_SEMANTIC_QUERY_TIMEOUT_MS,
            "values above the ceiling clamp to it"
        );
    }

    #[test]
    fn single_item_build_timeout_is_dead_evidence_without_same_batch_retry() {
        let (base_url, requests, handle) =
            start_slow_embedding_server(1, Duration::from_millis(300));
        let config = SemanticBackendConfig {
            backend: SemanticBackend::OpenAiCompatible,
            model: "test-embedding".to_string(),
            base_url: Some(base_url),
            api_key_env: None,
            timeout_ms: 100,
            query_timeout_ms: DEFAULT_SEMANTIC_QUERY_TIMEOUT_MS,
            max_batch_size: 64,
            max_files: 20_000,
            ..Default::default()
        };
        let mut model = SemanticEmbeddingModel::from_config(&config).unwrap();

        let error = model
            .embed(vec!["slow build batch".to_string()])
            .expect_err("a single item exceeding the base deadline is dead evidence");
        handle.join().expect("slow embedding server");

        assert!(embedding_failure_is_transient(&error), "error: {error}");
        assert!(
            error.contains("single-item request timed out at 100 ms: treating as down"),
            "error: {error}"
        );
        assert_eq!(
            requests.load(Ordering::SeqCst),
            1,
            "a timeout must shrink or terminate rather than retrying the same batch"
        );
    }

    fn programmable_http_config(server: &ProgrammableEmbeddingServer) -> SemanticBackendConfig {
        SemanticBackendConfig {
            backend: SemanticBackend::OpenAiCompatible,
            model: "test-embedding".to_string(),
            base_url: Some(server.base_url.clone()),
            api_key_env: None,
            // The floor leaves a single item on a loaded CI runner (HTTP setup
            // plus scheduling is tens of ms there) far below the base deadline:
            // a one-item timeout is the "down" verdict, and this suite must
            // reach it only from the never-answer arm, never from contention.
            timeout_ms: 300,
            query_timeout_ms: DEFAULT_SEMANTIC_QUERY_TIMEOUT_MS,
            max_batch_size: 64,
            max_files: 20_000,
            ..Default::default()
        }
    }

    fn embedding_inputs(count: usize) -> Vec<String> {
        (0..count).map(|index| format!("chunk {index}")).collect()
    }

    fn overflow_http_config(server: &OverflowEmbeddingServer) -> SemanticBackendConfig {
        SemanticBackendConfig {
            backend: SemanticBackend::OpenAiCompatible,
            model: "overflow-test-embedding".to_string(),
            base_url: Some(server.base_url.clone()),
            api_key_env: None,
            timeout_ms: 2_000,
            query_timeout_ms: DEFAULT_SEMANTIC_QUERY_TIMEOUT_MS,
            max_batch_size: 64,
            max_files: 20_000,
            ..Default::default()
        }
    }

    fn overflow_test_chunk(root: &Path, index: usize, embed_text: String) -> SemanticChunk {
        SemanticChunk {
            file: root.join(format!("src/file_{index}.rs")),
            name: format!("symbol_{index}"),
            qualified_name: None,
            kind: SymbolKind::Function,
            start_line: index as u32,
            end_line: index as u32 + 1,
            exported: false,
            embed_text,
            snippet: format!("fn symbol_{index}() {{}}"),
        }
    }

    fn build_chunks_with_model(
        root: &Path,
        chunks: Vec<SemanticChunk>,
        model: &mut SemanticEmbeddingModel,
    ) -> Result<SemanticIndex, String> {
        let file_metadata = chunks
            .iter()
            .map(|chunk| {
                (
                    chunk.file.clone(),
                    IndexedFileMetadata {
                        mtime: SystemTime::UNIX_EPOCH,
                        size: 0,
                        content_hash: blake3::hash(b""),
                    },
                )
            })
            .collect();
        let mut embed = |texts: Vec<String>| model.embed(texts);
        let mut should_continue = || true;
        SemanticIndex::build_from_chunks(
            root,
            chunks,
            file_metadata,
            &mut embed,
            64,
            Option::<&mut fn(usize, usize)>::None,
            &mut should_continue,
        )
    }

    #[test]
    fn overflow_build_bisects_and_shrinks_only_rejected_rows() {
        let root = tempfile::tempdir().expect("project root");
        let server = OverflowEmbeddingServer::rejecting_oversize(240);
        let config = overflow_http_config(&server);
        let mut model = SemanticEmbeddingModel::from_config(&config).unwrap();
        let oversize_indices = HashSet::from([7usize, 31, 58]);
        let chunks = (0..64)
            .map(|index| {
                let body = if oversize_indices.contains(&index) {
                    "dense-token ".repeat(80)
                } else {
                    format!("return value_{index};")
                };
                overflow_test_chunk(
                    root.path(),
                    index,
                    format!(
                        "name:symbol_{index} file:src/file_{index}.rs kind:function name:symbol_{index} signature:fn symbol_{index}() body:{body}"
                    ),
                )
            })
            .collect::<Vec<_>>();
        let originals = chunks
            .iter()
            .map(|chunk| (chunk.name.clone(), chunk.embed_text.clone()))
            .collect::<HashMap<_, _>>();

        let (result, events) = crate::logging::capture_index_events(|| {
            let (_guard, scope, mut failure_guard) = begin_semantic_index_build(root.path());
            let result = build_chunks_with_model(root.path(), chunks, &mut model);
            finish_semantic_index_build(&scope, &mut failure_guard, &result);
            result
        });
        let index = result.unwrap();

        assert_eq!(index.entry_count(), 64);
        assert_eq!(index.skipped_rows(), 0);
        assert!(
            events
                .iter()
                .any(|line| line.contains("kind=build_ready") && line.contains("skipped_rows=0")),
            "semantic ready event must disclose zero skipped rows: {events:?}",
        );
        let mut full_rows = 0usize;
        let mut shrunk_rows = 0usize;
        for entry in &index.entries {
            let original = originals.get(&entry.chunk.name).unwrap();
            let index = entry
                .chunk
                .name
                .strip_prefix("symbol_")
                .unwrap()
                .parse::<usize>()
                .unwrap();
            if oversize_indices.contains(&index) {
                assert!(entry.chunk.embed_text.len() < original.len());
                assert!(entry.chunk.embed_text.contains("name:symbol_"));
                shrunk_rows += 1;
            } else {
                assert_eq!(&entry.chunk.embed_text, original);
                full_rows += 1;
            }
            assert_eq!(entry.vector[0], entry.chunk.embed_text.len() as f32);
        }
        assert_eq!((full_rows, shrunk_rows), (61, 3));
        let restored = SemanticIndex::from_bytes(&index.to_bytes(), root.path()).unwrap();
        for entry in &restored.entries {
            assert_eq!(entry.vector[0], entry.chunk.embed_text.len() as f32);
            assert_eq!(
                entry.chunk.embed_text,
                index
                    .entries
                    .iter()
                    .find(|candidate| candidate.chunk.name == entry.chunk.name)
                    .unwrap()
                    .chunk
                    .embed_text,
            );
        }

        let request_count = server.requests().len();
        println!("overflow bisection request_count={request_count}");
        assert!(
            (8..40).contains(&request_count),
            "expected logarithmic bisection rather than 64 single-row probes; request_count={request_count}, request_sizes={:?}",
            server
                .requests()
                .iter()
                .map(Vec::len)
                .collect::<Vec<_>>(),
        );
    }

    #[test]
    fn overflow_build_skips_unshrinkable_row_and_reports_file() {
        let root = tempfile::tempdir().expect("project root");
        let server = OverflowEmbeddingServer::rejecting_oversize(240);
        let config = overflow_http_config(&server);
        let mut model = SemanticEmbeddingModel::from_config(&config).unwrap();
        let normal = overflow_test_chunk(
            root.path(),
            0,
            "name:normal file:src/normal.rs kind:function name:normal body:return one;".to_string(),
        );
        let unshrinkable = overflow_test_chunk(root.path(), 1, "A".repeat(3_000));
        let skipped_file = unshrinkable.file.display().to_string();
        take_test_skipped_row_warnings();

        let (result, events) = crate::logging::capture_index_events(|| {
            let (_guard, scope, mut failure_guard) = begin_semantic_index_build(root.path());
            let result =
                build_chunks_with_model(root.path(), vec![normal, unshrinkable], &mut model);
            finish_semantic_index_build(&scope, &mut failure_guard, &result);
            result
        });
        let index = result.unwrap();

        assert_eq!(index.entry_count(), 1);
        assert_eq!(index.skipped_rows(), 1);
        assert!(index
            .entries
            .iter()
            .all(|entry| entry.chunk.name != "symbol_1"));
        let warnings = take_test_skipped_row_warnings();
        assert_eq!(warnings.len(), 1, "warnings={warnings:?}");
        assert!(warnings[0].contains("semantic embed skipped row:"));
        assert!(warnings[0].contains(&format!("file={skipped_file}")));
        assert!(warnings[0].contains("symbol=symbol_1"));
        assert!(
            events
                .iter()
                .any(|line| line.contains("kind=build_ready") && line.contains("skipped_rows=1")),
            "semantic ready event must disclose skipped rows: {events:?}",
        );
    }

    #[test]
    fn unknown_4xx_still_aborts_semantic_build() {
        let root = tempfile::tempdir().expect("project root");
        let server = OverflowEmbeddingServer::rejecting_all(r#"{"error":"model not found"}"#);
        let config = overflow_http_config(&server);
        let mut model = SemanticEmbeddingModel::from_config(&config).unwrap();
        let chunk = overflow_test_chunk(
            root.path(),
            0,
            format!(
                "name:unknown file:src/unknown.rs kind:function name:unknown body:{}",
                "payload ".repeat(80)
            ),
        );

        let error = build_chunks_with_model(root.path(), vec![chunk], &mut model).unwrap_err();

        assert!(error.contains("HTTP 400 Bad Request"), "{error}");
        assert!(error.contains("model not found"), "{error}");
        assert!(!error.contains(ROW_TOO_LONG_MARKER_PREFIX), "{error}");
    }

    #[test]
    fn slow_backend_converges_without_being_marked_down() {
        // The reporter's shape (2 s/item against a 25 s floor) scaled so the
        // test exercises real HTTP deadlines in seconds, not minutes: a 64-item
        // batch cannot fit the initial deadline, a 4-item batch can.
        let server = ProgrammableEmbeddingServer::start(Duration::from_millis(50));
        let config = programmable_http_config(&server);
        let mut model = SemanticEmbeddingModel::from_config(&config).unwrap();

        for _ in 0..3 {
            assert_eq!(model.embed(embedding_inputs(64)).unwrap().len(), 64);
        }

        assert_eq!(model.adaptive_build_batch_size, config.max_batch_size);
        assert!(
            server.completed_sizes().contains(&64),
            "EMA-scaled deadlines must eventually let a recovered 64-item batch finish; requests={:?}, completed={:?}",
            server.request_sizes(),
            server.completed_sizes(),
        );
    }

    #[test]
    fn never_answering_backend_is_declared_down_within_eleven_base_deadlines() {
        let server = ProgrammableEmbeddingServer::start(Duration::ZERO);
        server.set_never_answer(true);
        let config = programmable_http_config(&server);
        let mut model = SemanticEmbeddingModel::from_config(&config).unwrap();
        let started = Instant::now();

        let error = model
            .embed(embedding_inputs(64))
            .expect_err("a backend that never answers must reach the singleton dead check");
        let elapsed = started.elapsed();

        assert!(
            error.contains("single-item request timed out at 300 ms: treating as down"),
            "error: {error}"
        );
        assert_eq!(server.request_sizes(), vec![64, 32, 16, 8, 4, 2, 1]);
        let protocol_bound = Duration::from_millis(config.timeout_ms * 11);
        assert!(
            elapsed <= protocol_bound + Duration::from_secs(1),
            "never-answer ladder exceeded 11 base deadlines plus scheduler allowance: elapsed={elapsed:?}, protocol_bound={protocol_bound:?}"
        );
    }

    #[test]
    fn refused_connection_is_an_immediate_honest_transient_failure() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("reserve refused port");
        let addr = listener.local_addr().expect("refused port address");
        drop(listener);
        let config = SemanticBackendConfig {
            backend: SemanticBackend::OpenAiCompatible,
            model: "test-embedding".to_string(),
            base_url: Some(format!("http://{addr}")),
            api_key_env: None,
            timeout_ms: 500,
            max_batch_size: 64,
            ..Default::default()
        };
        let mut model = SemanticEmbeddingModel::from_config(&config).unwrap();
        let started = Instant::now();

        let error = model
            .embed(vec!["connection probe".to_string()])
            .expect_err("closed listener must refuse the request");

        assert!(embedding_failure_is_transient(&error), "error: {error}");
        // Unix answers a closed loopback port with RST, so the request fails at
        // connect. Windows Filtering Platform stealth mode drops the SYN instead,
        // so the same probe is a connect timeout at the base floor - which the
        // one-item rule already reads as down. Either arm is one deadline at most
        // and never the same-batch retry ladder.
        if cfg!(windows) {
            assert!(
                error.contains(
                    "embedding backend unreachable (connection refused or connect failure)"
                ) || error.contains("single-item request timed out at 500 ms: treating as down"),
                "error: {error}"
            );
            assert!(
                started.elapsed() < Duration::from_millis(500 * 2),
                "a dropped SYN must be judged within one base deadline, not a ladder"
            );
        } else {
            assert!(
                error.contains(
                    "embedding backend unreachable (connection refused or connect failure)"
                ),
                "error: {error}"
            );
            assert!(
                started.elapsed() < Duration::from_millis(500),
                "connection refusal should not wait through a retry ladder"
            );
        }
    }

    #[test]
    fn aimd_grows_back_to_configured_max_after_backend_speeds_up() {
        let server = ProgrammableEmbeddingServer::start(Duration::from_millis(60));
        let config = programmable_http_config(&server);
        let mut model = SemanticEmbeddingModel::from_config(&config).unwrap();

        assert_eq!(model.embed(embedding_inputs(64)).unwrap().len(), 64);
        assert!(
            model.adaptive_build_batch_size < config.max_batch_size,
            "the initial slowdown should reduce the active batch size"
        );

        server.set_per_item_delay(Duration::from_millis(2));
        for _ in 0..4 {
            assert_eq!(model.embed(embedding_inputs(64)).unwrap().len(), 64);
            if model.adaptive_build_batch_size == config.max_batch_size {
                break;
            }
        }

        assert_eq!(model.adaptive_build_batch_size, config.max_batch_size);
    }

    #[test]
    fn openai_compatible_backend_embeds_with_mock_server() {
        let (base_url, handle) = start_mock_http_server(|request_line, path, _body| {
            assert!(request_line.starts_with("POST "));
            assert_eq!(path, "/v1/embeddings");
            "{\"data\":[{\"embedding\":[0.1,0.2,0.3],\"index\":0},{\"embedding\":[0.4,0.5,0.6],\"index\":1}]}".to_string()
        });

        let config = SemanticBackendConfig {
            backend: SemanticBackend::OpenAiCompatible,
            model: "test-embedding".to_string(),
            base_url: Some(base_url),
            api_key_env: None,
            timeout_ms: 5_000,
            query_timeout_ms: DEFAULT_SEMANTIC_QUERY_TIMEOUT_MS,
            max_batch_size: 64,
            max_files: 20_000,
            ..Default::default()
        };

        let mut model = SemanticEmbeddingModel::from_config(&config).unwrap();
        let vectors = model
            .embed(vec!["hello".to_string(), "world".to_string()])
            .unwrap();

        assert_eq!(vectors, vec![vec![0.1, 0.2, 0.3], vec![0.4, 0.5, 0.6]]);
        handle.join().unwrap();
    }

    /// Regression for issue #36: AFT was sending TWO Content-Type headers
    /// on the OpenAI embeddings request — once implicitly via `.json(&body)`
    /// and again explicitly via `.header("Content-Type", "application/json")`.
    /// reqwest's `.header()` calls `HeaderMap::append`, which produces two
    /// headers on the wire. OpenAI's /v1/embeddings endpoint rejects that
    /// with `HTTP 400 "you must provide a model parameter"` even though the
    /// body actually contains `model`. The fix is to drop the explicit
    /// `.header("Content-Type", ...)` call. This test pins that we send
    /// exactly one Content-Type header.
    #[test]
    fn openai_compatible_request_has_single_content_type_header() {
        use std::sync::{Arc, Mutex};
        let captured: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let captured_for_thread = Arc::clone(&captured);

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let addr = listener.local_addr().expect("local addr");
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            let mut header_end = None;
            let mut content_length = 0usize;
            loop {
                let n = stream.read(&mut chunk).expect("read");
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                if header_end.is_none() {
                    if let Some(pos) = buf.windows(4).position(|window| window == b"\r\n\r\n") {
                        header_end = Some(pos + 4);
                        for line in String::from_utf8_lossy(&buf[..pos + 4]).lines() {
                            if let Some(value) = line.strip_prefix("Content-Length:") {
                                content_length = value.trim().parse::<usize>().unwrap_or(0);
                            }
                        }
                    }
                }
                if let Some(end) = header_end {
                    if buf.len() >= end + content_length {
                        break;
                    }
                }
            }
            *captured_for_thread.lock().unwrap() = buf;
            let body = "{\"data\":[{\"embedding\":[0.1,0.2,0.3],\"index\":0}]}";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
        });

        let config = SemanticBackendConfig {
            backend: SemanticBackend::OpenAiCompatible,
            model: "text-embedding-3-small".to_string(),
            base_url: Some(format!("http://{}", addr)),
            api_key_env: None,
            timeout_ms: 5_000,
            query_timeout_ms: DEFAULT_SEMANTIC_QUERY_TIMEOUT_MS,
            max_batch_size: 64,
            max_files: 20_000,
            ..Default::default()
        };
        let mut model = SemanticEmbeddingModel::from_config(&config).unwrap();
        let _ = model.embed(vec!["probe".to_string()]).unwrap();
        handle.join().unwrap();

        let bytes = captured.lock().unwrap().clone();
        let request = String::from_utf8_lossy(&bytes);

        // Lowercase line counts because HTTP headers are case-insensitive
        // and reqwest may emit `content-type` in lowercase under HTTP/2.
        let content_type_lines = request
            .lines()
            .filter(|line| {
                let lower = line.to_ascii_lowercase();
                lower.starts_with("content-type:")
            })
            .count();
        assert_eq!(
            content_type_lines, 1,
            "expected exactly one Content-Type header but found {content_type_lines}; full request:\n{request}",
        );

        // The body must still include the model field — pin this so a future
        // change can't accidentally drop `model` while fixing duplicate headers.
        assert!(
            request.contains(r#""model":"text-embedding-3-small""#),
            "request body should contain model field; full request:\n{request}",
        );
    }

    #[test]
    fn ollama_backend_embeds_with_mock_server() {
        let (base_url, handle) = start_mock_http_server(|request_line, path, _body| {
            assert!(request_line.starts_with("POST "));
            assert_eq!(path, "/api/embed");
            "{\"embeddings\":[[0.7,0.8,0.9],[1.0,1.1,1.2]]}".to_string()
        });

        let config = SemanticBackendConfig {
            backend: SemanticBackend::Ollama,
            model: "embeddinggemma".to_string(),
            base_url: Some(base_url),
            api_key_env: None,
            timeout_ms: 5_000,
            query_timeout_ms: DEFAULT_SEMANTIC_QUERY_TIMEOUT_MS,
            max_batch_size: 64,
            max_files: 20_000,
            ..Default::default()
        };

        let mut model = SemanticEmbeddingModel::from_config(&config).unwrap();
        let vectors = model
            .embed(vec!["hello".to_string(), "world".to_string()])
            .unwrap();

        assert_eq!(vectors, vec![vec![0.7, 0.8, 0.9], vec![1.0, 1.1, 1.2]]);
        handle.join().unwrap();
    }

    #[test]
    fn read_from_disk_rejects_fingerprint_mismatch() {
        let storage = tempfile::tempdir().unwrap();
        let project_key = "proj";

        let project_root = test_project_root();
        let file = project_root.join("src/main.rs");
        let mut index = SemanticIndex::new(project_root.clone(), DEFAULT_DIMENSION);
        index.entries.push(EmbeddingEntry {
            chunk: SemanticChunk {
                file: file.clone(),
                name: "handle_request".to_string(),
                qualified_name: None,
                kind: SymbolKind::Function,
                start_line: 10,
                end_line: 25,
                exported: true,
                embed_text: "file:src/main.rs kind:function name:handle_request".to_string(),
                snippet: "fn handle_request() {}".to_string(),
            },
            norm: vector_norm(&[0.1, 0.2, 0.3]),
            vector: vec![0.1, 0.2, 0.3],
        });
        index.dimension = 3;
        index
            .file_mtimes
            .insert(file.clone(), SystemTime::UNIX_EPOCH);
        index.file_sizes.insert(file, 0);
        index.set_fingerprint(SemanticIndexFingerprint {
            backend: "openai_compatible".to_string(),
            model: "test-embedding".to_string(),
            base_url: "http://127.0.0.1:1234/v1".to_string(),
            dimension: 3,
            chunking_version: default_chunking_version(),
            ..Default::default()
        });
        index.write_to_disk(storage.path(), project_key);

        let data_path = storage
            .path()
            .join("semantic")
            .join(project_key)
            .join("semantic.bin");
        let before = fs::read(&data_path).unwrap();

        let matching = index.fingerprint().unwrap().as_string();
        assert!(SemanticIndex::read_from_disk(
            storage.path(),
            project_key,
            &project_root,
            false,
            Some(&matching),
        )
        .is_some());

        let mismatched = SemanticIndexFingerprint {
            backend: "ollama".to_string(),
            model: "embeddinggemma".to_string(),
            base_url: "http://127.0.0.1:11434".to_string(),
            dimension: 3,
            chunking_version: default_chunking_version(),
            ..Default::default()
        }
        .as_string();
        assert!(SemanticIndex::read_from_disk(
            storage.path(),
            project_key,
            &project_root,
            false,
            Some(&mismatched),
        )
        .is_none());
        assert_eq!(fs::read(&data_path).unwrap(), before);
    }

    #[test]
    fn synapse_fingerprint_pin_matches_only_equivalent_alias_at_same_epoch() {
        let cached = SemanticIndexFingerprint {
            backend: "synapse".to_string(),
            model: "configured-model".to_string(),
            dimension: 768,
            chunking_version: 2,
            synapse_fingerprint: Some("fp-old".to_string()),
            synapse_table_epoch: Some(9),
            ..Default::default()
        };
        let mut served = cached.clone();
        served.synapse_fingerprint = Some("fp-current".to_string());
        served.synapse_equivalent_to = vec!["fp-old".to_string()];
        assert!(cached.matches_expected(&served.as_string()));

        served.synapse_table_epoch = Some(10);
        assert!(!cached.matches_expected(&served.as_string()));
    }

    #[test]
    fn fingerprint_mismatch_details_redact_base_url_and_list_changed_fields() {
        let cached = SemanticIndexFingerprint {
            backend: "openai_compatible".to_string(),
            model: "cached-model".to_string(),
            base_url: "https://user:secret@example.com/v1/embeddings".to_string(),
            dimension: 3,
            chunking_version: 2,
            ..Default::default()
        };
        let current = SemanticIndexFingerprint {
            backend: "ollama".to_string(),
            model: "current-model".to_string(),
            base_url: "https://example.org/api/embed".to_string(),
            dimension: 4,
            chunking_version: 3,
            ..Default::default()
        };

        let details = format_fingerprint_mismatch_details(Some(&cached), &current);

        assert!(details.contains("backend kind cached=openai_compatible current=ollama"));
        assert!(details.contains("model cached=cached-model current=current-model"));
        assert!(details.contains("base_url host cached=example.com current=example.org"));
        assert!(details.contains("dimension cached=3 current=4"));
        assert!(details.contains("chunking version cached=2 current=3"));
        assert!(!details.contains("secret"));
        assert!(!details.contains("/v1/embeddings"));
        assert!(!details.contains("/api/embed"));
    }

    #[test]
    fn read_from_disk_rejects_v3_cache_for_snippet_rebuild() {
        let storage = tempfile::tempdir().unwrap();
        let project_key = "proj-v3";
        let dir = storage.path().join("semantic").join(project_key);
        fs::create_dir_all(&dir).unwrap();

        let mut index = SemanticIndex::new(test_project_root(), DEFAULT_DIMENSION);
        index.entries.push(EmbeddingEntry {
            chunk: SemanticChunk {
                file: PathBuf::from("/src/main.rs"),
                name: "handle_request".to_string(),
                qualified_name: None,
                kind: SymbolKind::Function,
                start_line: 0,
                end_line: 0,
                exported: true,
                embed_text: "file:src/main.rs kind:function name:handle_request".to_string(),
                snippet: "fn handle_request() {}".to_string(),
            },
            norm: vector_norm(&[0.1, 0.2, 0.3]),
            vector: vec![0.1, 0.2, 0.3],
        });
        index.dimension = 3;
        index
            .file_mtimes
            .insert(PathBuf::from("/src/main.rs"), SystemTime::UNIX_EPOCH);
        index.file_sizes.insert(PathBuf::from("/src/main.rs"), 0);
        let fingerprint = SemanticIndexFingerprint {
            backend: "fastembed".to_string(),
            model: "test".to_string(),
            base_url: FALLBACK_BACKEND.to_string(),
            dimension: 3,
            chunking_version: default_chunking_version(),
            ..Default::default()
        };
        index.set_fingerprint(fingerprint.clone());

        let mut bytes = index.to_bytes();
        bytes[0] = SEMANTIC_INDEX_VERSION_V3;
        let data_path = dir.join("semantic.bin");
        fs::write(&data_path, &bytes).unwrap();

        assert!(SemanticIndex::read_from_disk(
            storage.path(),
            project_key,
            &test_project_root(),
            false,
            Some(&fingerprint.as_string())
        )
        .is_none());
        assert_eq!(fs::read(&data_path).unwrap(), bytes);
    }

    fn make_symbol(kind: SymbolKind, name: &str, start: u32, end: u32) -> crate::symbols::Symbol {
        crate::symbols::Symbol {
            name: name.to_string(),
            kind,
            range: crate::symbols::Range {
                start_line: start,
                start_col: 0,
                end_line: end,
                end_col: 0,
            },
            signature: None,
            scope_chain: Vec::new(),
            exported: false,
            parent: None,
        }
    }

    #[test]
    fn symbols_to_chunks_sets_qualified_name_without_changing_embed_text() {
        let project_root = PathBuf::from("/proj");
        let file = project_root.join("src/engine.ts");
        let source = "class Index {\n}\n";
        let mut symbol = make_symbol(SymbolKind::Class, "Index", 0, 1);
        symbol.scope_chain = vec!["Engine".to_string()];
        symbol.signature = Some("class Index".to_string());
        let embed_text = build_embed_text(&symbol, source, &file, &project_root);

        let chunks = symbols_to_chunks(&file, &[symbol], source, &project_root);
        let chunk = chunks
            .iter()
            .find(|chunk| chunk.name == "Index")
            .expect("class chunk");

        assert_eq!(chunk.name, "Index");
        assert_eq!(chunk.qualified_name.as_deref(), Some("Engine.Index"));
        assert_eq!(chunk.embed_text, embed_text);
        assert!(!chunk.embed_text.contains("Engine.Index"));
    }

    /// Heading symbols (Markdown / HTML headings) must NOT be indexed —
    /// they overwhelmingly dominated semantic results even on code-shaped
    /// queries because heading prose embeds far more strongly than code
    /// chunks. Skipping headings keeps aft_search a code-finder.
    #[test]
    fn symbols_to_chunks_skips_heading_symbols() {
        let project_root = PathBuf::from("/proj");
        let file = project_root.join("README.md");
        let source = "# Title\n\nbody text\n\n## Section\n\nmore text\n";

        let symbols = vec![
            make_symbol(SymbolKind::Heading, "Title", 0, 2),
            make_symbol(SymbolKind::Heading, "Section", 4, 6),
        ];

        let chunks = symbols_to_chunks(&file, &symbols, source, &project_root);
        assert!(
            chunks.is_empty(),
            "Heading symbols must be filtered out before embedding; got {} chunk(s)",
            chunks.len()
        );
    }

    /// A symbol with an enormous signature (e.g. a YAML/Kubernetes CronJob
    /// whose inline `command:` script is parsed into the signature) must not
    /// produce an embed_text that overflows the embedding backend's physical
    /// batch. Before the clamp, the unbounded `signature:` append created a
    /// multi-KB input that aborted the whole index build and degraded every
    /// search to lexical-only.
    #[test]
    fn build_embed_text_clamps_oversized_signature() {
        let project_root = PathBuf::from("/proj");
        let file = project_root.join("cronjob.yaml");
        let huge_sig = "kubectl ".repeat(2000); // ~16 KB
        let source = "apiVersion: batch/v1\nkind: CronJob\n";

        let mut symbol = make_symbol(SymbolKind::Class, "cluster-janitor", 0, 1);
        symbol.signature = Some(huge_sig);

        let text = build_embed_text(&symbol, source, &file, &project_root);
        assert!(
            text.chars().count() <= MAX_EMBED_TEXT_CHARS,
            "embed_text must be clamped to {} chars, got {}",
            MAX_EMBED_TEXT_CHARS,
            text.chars().count()
        );
    }

    #[test]
    fn embed_text_caps_resolve_per_backend_without_changing_defaults() {
        let defaults = EmbedTextCaps::default();

        let mut local = SemanticBackendConfig {
            max_input_tokens: Some(960),
            ..SemanticBackendConfig::default()
        };
        assert_eq!(EmbedTextCaps::from_config(&local), defaults);

        local.backend = SemanticBackend::OpenAiCompatible;
        local.base_url = Some("http://127.0.0.1:1234/v1".to_string());
        local.max_input_tokens = None;
        assert_eq!(EmbedTextCaps::from_config(&local), defaults);

        local.max_input_tokens = Some(531);
        let expanded = EmbedTextCaps::from_config(&local);
        assert_eq!(expanded.signature_chars, 400);
        assert_eq!(expanded.body_lines, usize::MAX);
        assert_eq!(expanded.body_chars, 1001);
        assert_eq!(expanded.total_chars, 1858);

        for backend in [SemanticBackend::Ollama, SemanticBackend::Synapse] {
            local.backend = backend;
            assert_eq!(EmbedTextCaps::from_config(&local), expanded);
        }
    }

    #[test]
    fn semantic_fingerprint_changes_when_embed_text_caps_change() {
        let mut config = SemanticBackendConfig {
            backend: SemanticBackend::OpenAiCompatible,
            model: "test-embedding".to_string(),
            base_url: Some("http://127.0.0.1:1234/v1".to_string()),
            ..SemanticBackendConfig::default()
        };
        let legacy = SemanticIndexFingerprint::for_config_dimension(&config, 1024);

        config.max_input_tokens = Some(531);
        let expanded = SemanticIndexFingerprint::for_config_dimension(&config, 1024);

        assert_ne!(legacy.embed_text_caps, expanded.embed_text_caps);
        assert_ne!(legacy.as_string(), expanded.as_string());
        assert!(!legacy.matches(&expanded));
    }

    #[test]
    fn file_summary_embed_text_is_independent_of_symbol_caps() {
        let project_root = PathBuf::from("/proj");
        let file = project_root.join("src/long.rs");
        let source = "//! module docs\npub fn exported() {}\n";
        let mut symbol = make_symbol(SymbolKind::Function, "exported", 1, 1);
        symbol.exported = true;
        symbol.signature = Some("pub fn exported()".to_string());

        let legacy = symbols_to_chunks_with_caps(
            &file,
            std::slice::from_ref(&symbol),
            source,
            &project_root,
            EmbedTextCaps::default(),
        );
        let expanded = symbols_to_chunks_with_caps(
            &file,
            &[symbol],
            source,
            &project_root,
            EmbedTextCaps {
                signature_chars: 400,
                body_lines: usize::MAX,
                body_chars: 2500,
                total_chars: 3357,
            },
        );

        let legacy_summary = legacy
            .iter()
            .find(|chunk| chunk.kind == SymbolKind::FileSummary)
            .expect("legacy file summary");
        let expanded_summary = expanded
            .iter()
            .find(|chunk| chunk.kind == SymbolKind::FileSummary)
            .expect("expanded file summary");
        assert_eq!(legacy_summary.embed_text, expanded_summary.embed_text);
    }

    #[test]
    fn unbounded_chunk_caps_preserve_full_signature_and_body() {
        let project_root = PathBuf::from("/proj");
        let file = project_root.join("long.rs");
        let source = (0..20)
            .map(|line| format!("line_{line:02}_{}", "body".repeat(20)))
            .collect::<Vec<_>>()
            .join("\n");
        let mut symbol = make_symbol(SymbolKind::Function, "long_function", 0, 19);
        symbol.signature = Some(format!(
            "fn long_function({}) SIGNATURE_END",
            "x".repeat(500)
        ));
        let line_cache = SourceLineCache::new(&source);

        let today = build_embed_text_with_lines_and_caps(
            &symbol,
            &line_cache,
            &file,
            &project_root,
            EmbedTextCaps::default(),
        );
        let full = build_embed_text_with_lines_and_caps(
            &symbol,
            &line_cache,
            &file,
            &project_root,
            EmbedTextCaps {
                signature_chars: usize::MAX,
                body_lines: usize::MAX,
                body_chars: usize::MAX,
                total_chars: usize::MAX,
            },
        );

        assert!(!today.contains("SIGNATURE_END"));
        assert!(!today.contains("line_19"));
        assert!(full.contains("SIGNATURE_END"));
        assert!(full.contains("line_19"));
        assert!(full.len() > today.len());
    }

    /// Code symbols (functions, classes, methods, structs, etc.) must still
    /// be indexed alongside the heading skip — otherwise we'd starve the
    /// index entirely.
    #[test]
    fn symbols_to_chunks_keeps_code_symbols_alongside_skipped_headings() {
        let project_root = PathBuf::from("/proj");
        let file = project_root.join("src/lib.rs");
        let source = "pub fn handle_request() -> bool {\n    true\n}\n";

        let symbols = vec![
            // A heading mixed in (e.g. from a doc comment block elsewhere).
            make_symbol(SymbolKind::Heading, "doc heading", 0, 1),
            make_symbol(SymbolKind::Function, "handle_request", 0, 2),
            make_symbol(SymbolKind::Struct, "AuthService", 4, 6),
        ];

        let chunks = symbols_to_chunks(&file, &symbols, source, &project_root);
        assert_eq!(
            chunks.len(),
            3,
            "Expected file-summary + 2 code chunks (Function + Struct), got {}",
            chunks.len()
        );
        let names: Vec<&str> = chunks.iter().map(|c| c.name.as_str()).collect();
        assert!(chunks
            .iter()
            .any(|chunk| matches!(chunk.kind, SymbolKind::FileSummary)));
        assert!(names.contains(&"handle_request"));
        assert!(names.contains(&"AuthService"));
        assert!(
            !names.contains(&"doc heading"),
            "Heading symbol leaked into chunks: {names:?}"
        );
    }

    #[test]
    fn validate_ssrf_allows_loopback_hostnames() {
        // Loopback hostnames are explicitly allowed so self-hosted backends
        // (Ollama at http://localhost:11434) work at their default config.
        for host in &[
            "http://localhost",
            "http://localhost:8080",
            "http://localhost:11434", // Ollama default
            "http://localhost.localdomain",
            "http://foo.localhost",
        ] {
            assert!(
                validate_base_url_no_ssrf(host).is_ok(),
                "Expected {host} to be allowed (loopback), got: {:?}",
                validate_base_url_no_ssrf(host)
            );
        }
    }

    #[test]
    fn validate_ssrf_allows_loopback_ips() {
        // 127.0.0.0/8 is loopback — by definition same-machine and not an
        // SSRF target. Allow it so Ollama at http://127.0.0.1:11434 works.
        for url in &[
            "http://127.0.0.1",
            "http://127.0.0.1:11434", // Ollama default
            "http://127.0.0.1:8080",
            "http://127.1.2.3",
        ] {
            let result = validate_base_url_no_ssrf(url);
            assert!(
                result.is_ok(),
                "Expected {url} to be allowed (loopback), got: {:?}",
                result
            );
        }
    }

    #[test]
    fn validate_ssrf_rejects_private_non_loopback_ips() {
        // Non-loopback private/reserved IPs remain rejected — homelab/intranet
        // services on LAN IPs are real SSRF targets even though the user
        // configured them. Users who want this can opt in by binding the
        // service to a public-routable address.
        for url in &[
            "http://192.168.1.1",
            "http://10.0.0.1",
            "http://172.16.0.1",
            "http://169.254.169.254",
            "http://100.64.0.1",
        ] {
            let result = validate_base_url_no_ssrf(url);
            assert!(
                result.is_err(),
                "Expected {url} to be rejected (non-loopback private), got: {:?}",
                result
            );
        }
    }

    #[test]
    fn validate_ssrf_rejects_mdns_local_hostnames() {
        // mDNS .local hostnames typically resolve to LAN devices, not
        // loopback. Rejecting them before DNS lookup gives a clearer error.
        for host in &[
            "http://printer.local",
            "http://nas.local:8080",
            "http://homelab.local",
        ] {
            let result = validate_base_url_no_ssrf(host);
            assert!(
                result.is_err(),
                "Expected {host} to be rejected (mDNS), got: {:?}",
                result
            );
        }
    }

    #[test]
    fn normalize_base_url_allows_localhost_for_tests() {
        // normalize_base_url itself should NOT block localhost — only
        // validate_base_url_no_ssrf does. Tests construct backends directly.
        assert!(normalize_base_url("http://127.0.0.1:9999").is_ok());
        assert!(normalize_base_url("http://localhost:8080").is_ok());
    }

    #[test]
    fn ssrf_guard_blocks_reserved_ranges_but_allows_loopback() {
        use std::net::IpAddr;
        let blocked = |s: &str| is_private_non_loopback_ip(&s.parse::<IpAddr>().unwrap());

        // Private / link-local / CGNAT — blocked (unchanged behavior).
        assert!(blocked("10.0.0.1"));
        assert!(blocked("192.168.1.1"));
        assert!(blocked("169.254.0.1"));
        assert!(blocked("100.64.0.1"));
        // Newly covered by delegating to url_fetch's complete list:
        assert!(
            blocked("198.18.0.1"),
            "RFC2544 benchmark range must be blocked"
        );
        assert!(blocked("224.0.0.1"), "multicast must be blocked");
        assert!(blocked("fc00::1"), "IPv6 ULA must be blocked");
        assert!(blocked("fe80::1"), "IPv6 link-local must be blocked");

        // Loopback — allowed (local Ollama endpoint), incl. IPv4-mapped form.
        assert!(!blocked("127.0.0.1"), "loopback must stay allowed");
        assert!(!blocked("::1"), "IPv6 loopback must stay allowed");
        assert!(
            !blocked("::ffff:127.0.0.1"),
            "IPv4-mapped loopback must stay allowed (matches prior carve-out)"
        );

        // A public address must NOT be flagged.
        assert!(!blocked("8.8.8.8"));
    }

    /// Pin the user-facing wording of the ONNX version-mismatch error.
    /// The auto-fix path MUST be listed first because it's the only safe
    /// option that doesn't require sudo or risk breaking other apps that
    /// link the system library. Regression of any of these strings would
    /// either mislead users (system rm before auto-fix) or break the
    /// `aft doctor --fix` discovery path.
    #[test]
    fn ort_mismatch_message_recommends_auto_fix_first() {
        let msg =
            format_ort_version_mismatch("1.9.0", "/usr/lib/x86_64-linux-gnu/libonnxruntime.so");

        // The reported version and path must appear verbatim.
        assert!(
            msg.contains("v1.9.0"),
            "should report detected version: {msg}"
        );
        assert!(
            msg.contains("/usr/lib/x86_64-linux-gnu/libonnxruntime.so"),
            "should report system path: {msg}"
        );
        assert!(msg.contains("v1.20+"), "should state requirement: {msg}");

        // Solution ordering: auto-fix is #1, system rm is #2, install is #3.
        let auto_fix_pos = msg
            .find("Auto-fix")
            .expect("Auto-fix solution missing — users won't discover --fix");
        let remove_pos = msg
            .find("Remove the old library")
            .expect("system-rm solution missing");
        assert!(
            auto_fix_pos < remove_pos,
            "Auto-fix must come before manual rm — see PR comment thread"
        );

        // The auto-fix command must be runnable as-is on a fresh system.
        assert!(
            msg.contains("npx @cortexkit/aft doctor --fix"),
            "auto-fix command must be present and copy-pasteable: {msg}"
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn loaded_ort_version_detection_prefers_actual_loaded_library_path() {
        let requested = "libonnxruntime.so";
        let actual = "/usr/local/lib/libonnxruntime.so.1.19.0";

        assert_eq!(detect_ort_version_from_path(requested), None);
        let (version, source) =
            detect_ort_version_from_resolved_or_requested(Some(actual.to_string()), requested);

        assert_eq!(version, Some("1.19.0".to_string()));
        assert_eq!(source, actual);

        let msg = format_ort_version_mismatch(&version.unwrap(), &source);
        assert!(msg.contains("v1.19.0"));
        assert!(msg.contains(actual));
    }

    /// macOS dylib paths must not produce a malformed message when the
    /// system path lacks a trailing slash. This is a regression guard
    /// for the "{}\n{}" format string contract.
    #[test]
    fn ort_mismatch_message_handles_macos_dylib_path() {
        let msg = format_ort_version_mismatch("1.9.0", "/opt/homebrew/lib/libonnxruntime.dylib");
        assert!(msg.contains("v1.9.0"));
        assert!(msg.contains("/opt/homebrew/lib/libonnxruntime.dylib"));
        // The dylib path must appear in the auto-fix paragraph (single
        // quotes around it) AND in the manual-rm paragraph; verify
        // both placements survived the format string.
        assert!(
            msg.contains("'/opt/homebrew/lib/libonnxruntime.dylib'"),
            "system path should be quoted in the auto-fix sentence: {msg}"
        );
    }

    // ── managed ONNX Runtime resolver tests ──────────────────────────────────

    /// Build a fake `<storage>/onnxruntime/<version>/<libname>` tree. Returns
    /// the storage root. `lib_name` is the platform library filename the
    /// resolver looks for.
    fn fake_managed_ort_tree(storage: &std::path::Path, lib_name: &str, versions: &[(&str, bool)]) {
        for (version, has_lib) in versions {
            let dir = storage.join("onnxruntime").join(version);
            std::fs::create_dir_all(&dir).unwrap();
            if *has_lib {
                std::fs::write(dir.join(lib_name), b"fake-ort").unwrap();
            }
        }
    }

    #[test]
    fn managed_ort_resolver_picks_highest_compatible_version() {
        let _env_lock = crate::test_env::process_env_lock();
        let storage = tempfile::tempdir().unwrap();
        fake_managed_ort_tree(
            storage.path(),
            MANAGED_ORT_LIB_NAME,
            &[
                ("1.19.0", true), // below the 1.20 floor — must be ignored
                ("1.20.1", true),
                ("1.24.4", true), // highest compatible — must win
                ("1.23.0", true),
            ],
        );
        let found = find_managed_onnx_runtime(storage.path()).expect("resolver finds a runtime");
        assert_eq!(
            found,
            storage
                .path()
                .join("onnxruntime")
                .join("1.24.4")
                .join(MANAGED_ORT_LIB_NAME)
        );
    }

    #[test]
    fn managed_ort_resolver_ignores_non_version_and_pre_120_dirs() {
        let _env_lock = crate::test_env::process_env_lock();
        let storage = tempfile::tempdir().unwrap();
        fake_managed_ort_tree(
            storage.path(),
            MANAGED_ORT_LIB_NAME,
            &[
                ("1.19.0", true),     // pre-1.20 — ignored
                ("1.24.4.tmp", true), // not a parseable version — ignored
                ("latest", true),     // not a version — ignored
                ("1.24.4", false),    // compatible but no library file — ignored
            ],
        );
        assert_eq!(
            find_managed_onnx_runtime(storage.path()),
            None,
            "no compatible version with a library file should resolve"
        );
    }

    #[test]
    fn empty_onnx_runtime_override_is_unset_with_an_injected_lookup() {
        assert!(!onnx_runtime_override_configured_with(|key| {
            assert_eq!(key, "ORT_DYLIB_PATH");
            Some(std::ffi::OsString::new())
        }));
        assert!(onnx_runtime_override_configured_with(|_| Some(
            std::ffi::OsString::from("/runtime/libonnxruntime.so")
        )));
    }

    #[test]
    fn managed_ort_resolver_absent_tree_falls_through() {
        let _env_lock = crate::test_env::process_env_lock();
        let storage = tempfile::tempdir().unwrap();
        // No onnxruntime/ dir at all.
        assert_eq!(find_managed_onnx_runtime(storage.path()), None);
        // Empty onnxruntime/ dir.
        std::fs::create_dir_all(storage.path().join("onnxruntime")).unwrap();
        assert_eq!(find_managed_onnx_runtime(storage.path()), None);
    }

    #[test]
    fn managed_ort_resolver_prefers_version_root_over_lib_subdir() {
        let _env_lock = crate::test_env::process_env_lock();
        let storage = tempfile::tempdir().unwrap();
        let version_dir = storage.path().join("onnxruntime").join("1.24.4");
        std::fs::create_dir_all(version_dir.join("lib")).unwrap();
        // Both the version root and the lib/ subdir hold the library; the root
        // must win (mirrors resolveCachedOnnxRuntimeDir).
        std::fs::write(version_dir.join(MANAGED_ORT_LIB_NAME), b"root").unwrap();
        std::fs::write(version_dir.join("lib").join(MANAGED_ORT_LIB_NAME), b"lib").unwrap();
        let found = find_managed_onnx_runtime(storage.path()).expect("resolver finds a runtime");
        assert_eq!(found, version_dir.join(MANAGED_ORT_LIB_NAME));
    }

    #[test]
    fn managed_ort_resolver_accepts_lib_subdir_only() {
        let _env_lock = crate::test_env::process_env_lock();
        let storage = tempfile::tempdir().unwrap();
        let version_dir = storage.path().join("onnxruntime").join("1.24.4");
        std::fs::create_dir_all(version_dir.join("lib")).unwrap();
        // Library only under lib/ (manual Microsoft-archive install, #71).
        std::fs::write(version_dir.join("lib").join(MANAGED_ORT_LIB_NAME), b"lib").unwrap();
        let found = find_managed_onnx_runtime(storage.path()).expect("resolver finds a runtime");
        assert_eq!(found, version_dir.join("lib").join(MANAGED_ORT_LIB_NAME));
    }

    #[test]
    fn managed_ort_resolver_pre_set_env_short_circuits_without_reading_tree() {
        let _env_lock = crate::test_env::process_env_lock();
        let storage = tempfile::tempdir().unwrap();
        // Plant a poison dir that would panic the resolver if it were read:
        // a version dir whose name is a valid version but whose library file is
        // a directory (so `is_file()` would be false) — harmless, but the point
        // is the resolver must never even look.
        let poison = storage.path().join("onnxruntime").join("1.24.4");
        std::fs::create_dir_all(poison.join(MANAGED_ORT_LIB_NAME)).unwrap();

        let before = MANAGED_ORT_PROBE_READS.load(Ordering::Relaxed);
        // Pre-set ORT_DYLIB_PATH — the resolver must not run at all.
        std::env::set_var("ORT_DYLIB_PATH", "/explicit/override/libonnxruntime.so");
        resolve_managed_onnx_runtime(storage.path());
        std::env::remove_var("ORT_DYLIB_PATH");
        assert_eq!(
            MANAGED_ORT_PROBE_READS.load(Ordering::Relaxed),
            before,
            "resolver must not read the storage tree when ORT_DYLIB_PATH is pre-set"
        );
    }

    #[test]
    fn cancelled_build_stops_before_the_next_embed_batch() {
        let project = tempfile::tempdir().expect("project directory");
        let files = (0..16)
            .map(|index| {
                let path = project.path().join(format!("batch_{index}.rs"));
                std::fs::write(&path, format!("pub fn batch_symbol_{index}() {{}}\n"))
                    .expect("write source");
                path
            })
            .collect::<Vec<_>>();
        let cancelled = std::sync::atomic::AtomicBool::new(false);
        let embed_calls = AtomicUsize::new(0);
        let total_chunks = AtomicUsize::new(0);
        let mut embed = |texts: Vec<String>| {
            let call = embed_calls.fetch_add(1, Ordering::SeqCst) + 1;
            assert_eq!(texts.len(), 1, "one chunk per mocked batch");
            if call == 1 {
                cancelled.store(true, Ordering::SeqCst);
            }
            Ok(vec![vec![1.0, 2.0, 3.0]])
        };
        let mut progress = |done: usize, total: usize| {
            assert!(done <= total);
            total_chunks.store(total, Ordering::SeqCst);
        };
        let mut should_continue = || !cancelled.load(Ordering::SeqCst);

        let error = SemanticIndex::build_with_progress_and_cancellation(
            project.path(),
            &files,
            &mut embed,
            1,
            &mut progress,
            &mut should_continue,
        )
        .expect_err("the second batch boundary observes cancellation");

        let total_chunks = total_chunks.load(Ordering::SeqCst);
        assert!(error.contains("semantic build superseded"));
        assert_eq!(embed_calls.load(Ordering::SeqCst), 1);
        assert!(
            total_chunks > 4,
            "fixture must contain enough chunks to demonstrate an early stop, got {total_chunks}"
        );
    }

    #[test]
    fn managed_ort_resolver_sets_env_when_found() {
        let _env_lock = crate::test_env::process_env_lock();
        let storage = tempfile::tempdir().unwrap();
        fake_managed_ort_tree(storage.path(), MANAGED_ORT_LIB_NAME, &[("1.24.4", true)]);
        std::env::remove_var("ORT_DYLIB_PATH");
        resolve_managed_onnx_runtime(storage.path());
        let set = std::env::var_os("ORT_DYLIB_PATH").expect("resolver sets ORT_DYLIB_PATH");
        assert_eq!(
            PathBuf::from(set),
            storage
                .path()
                .join("onnxruntime")
                .join("1.24.4")
                .join(MANAGED_ORT_LIB_NAME)
        );
        std::env::remove_var("ORT_DYLIB_PATH");
    }
}

#[cfg(test)]
mod embedding_backend_retry_status_tests {
    use super::*;

    fn unique_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("aft-backend-status-{name}-{}", std::process::id()))
    }

    // The parked status must survive the start of the retry that will re-probe
    // the backend: the retry re-walks and re-chunks the corpus before it reaches
    // the backend, and on a large root that window is most of the cycle. On the
    // 2026-09-19 card the status was cleared at `begin`, so health read
    // "building" for every retry window and the sentinel fired index.stuck.
    #[test]
    fn parked_status_survives_the_start_of_a_retry() {
        let root = unique_root("survives-begin");
        record_embedding_backend_build_failure_for_test(&root, "connection refused");
        let (_guard, _scope, _failure_guard) = begin_semantic_index_build(&root);
        let health = embedding_backend_build_health(&root)
            .expect("parked status was cleared by the retry's own begin");
        assert_eq!(health.last_error, "connection refused");
        clear_embedding_backend_build_failure(&root);
    }

    // The deadline health reports is the one the loop sleeps to. The builder's
    // own record runs first with its schedule position; the loop then stamps
    // the backoff it computed, so a loop on its fourth attempt (60 s) never
    // shows the builder's first-attempt 15 s.
    #[test]
    fn loop_deadline_overrides_the_builder_schedule_position() {
        let root = unique_root("loop-deadline");
        record_embedding_backend_build_failure_for_test(&root, "connection refused");
        let before = unix_millis_now();
        record_embedding_backend_retry_deadline(
            &root,
            "connection refused",
            std::time::Duration::from_secs(60),
        );
        let health = embedding_backend_build_health(&root).expect("status recorded");
        assert!(
            health.next_retry_ms >= before + 60_000,
            "deadline {} is not the loop's 60 s backoff from {before}",
            health.next_retry_ms
        );
        clear_embedding_backend_build_failure(&root);
    }

    // A parked status must not outlive the loop that would retry it.
    #[test]
    fn abandoned_loop_clears_the_parked_status() {
        let root = unique_root("abandoned");
        record_embedding_backend_build_failure_for_test(&root, "connection refused");
        clear_embedding_backend_retry_status(&root);
        assert!(embedding_backend_build_health(&root).is_none());
    }
}
