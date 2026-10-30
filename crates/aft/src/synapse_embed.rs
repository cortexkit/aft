use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use subc_client_rs::{
    CallError, CallOptions, ConsumerOptions, OutcomeUnknownCause, RouteHandle, SubcConsumer,
};
use subc_protocol::manifest::ProviderRole;
use subc_protocol::{BindIdentity, RouteTarget};

use crate::config::SemanticBackendConfig;

const SYNAPSE_MODULE_ID: &str = "synapse";
const MODELS_LIST_OPERATION: &str = "models.list";
const QUERY_OPERATION: &str = "embed.query";
const BATCH_OPERATION: &str = "embed.batch";
const MAX_RESULT_PAGE_BYTES: usize = 512 * 1024;
const MAX_CALL_ATTEMPTS: usize = 4;
const MAX_NO_PROGRESS_POLLS: usize = 5;
const CIRCUIT_TIMEOUT_THRESHOLD: usize = 3;
const CIRCUIT_COOLDOWN: Duration = Duration::from_secs(5);
const RETRY_BACKOFF_MS: [u64; 3] = [100, 200, 400];
static LIVE_CAPTURE_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

#[cfg(test)]
#[path = "synapse_route_tests.rs"]
pub(crate) mod route_tests;

const LANE_PAUSE_MARKER: &str = "[lane_paused] ";
pub(crate) const MAX_LANE_PAUSE_MS: u64 = 120 * 60 * 1_000;
/// Shortest retry hint honoured from a reply of class `transient`. Synapse
/// answers a loading model with 250 ms, so the `lane_paused` floor of one
/// second would discard real hints; anything shorter uses the normal backoff.
pub(crate) const MIN_TRANSIENT_RETRY_MS: u64 = 100;

/// Why Synapse refused an embedding request for now. Every cause travels the
/// same pause path: background indexing waits and retries while keeping its
/// index, queries fall back to keyword search at once, and none of them counts
/// as a model failure or a circuit-breaker timeout.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PauseCause {
    /// `lane_paused`: an announced, scheduled pause of the model lane.
    #[default]
    LanePaused,
    /// `shape_compiling` or `model_loading`: the model is still warming up and
    /// will serve the same request once it is ready.
    WarmingUp,
    /// Any other reply of class `transient` (a full queue, an expired
    /// deadline, a restarting engine, a quarantined lane).
    Transient,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LanePause {
    pub lane_id: String,
    pub fingerprint: Option<String>,
    pub retry_after_ms: Option<u64>,
    pub paused_until_ms: u64,
    pub reason: String,
    #[serde(default)]
    pub cause: PauseCause,
    /// Synapse's `safe_to_retry_same_request`. When false, a retry must not
    /// resend the refused request unchanged; see
    /// [`LanePause::retry_same_request`].
    #[serde(default = "same_request_is_safe")]
    pub safe_to_retry_same_request: bool,
    /// Which recorded pause this is: set when the client records the
    /// refusal for its route root, and unique within the process, so a
    /// worker's retry time reaches only the pause it waits on. 0 until then.
    #[serde(default)]
    pub record_id: u64,
}

fn same_request_is_safe() -> bool {
    true
}

impl LanePause {
    pub fn from_error(error: &str) -> Option<Self> {
        let (_, payload) = error.split_once(LANE_PAUSE_MARKER)?;
        serde_json::Deserializer::from_str(payload)
            .into_iter::<Self>()
            .next()?
            .ok()
    }

    pub fn retry_delay(&self, backoff: Duration) -> Duration {
        backoff.max(Duration::from_millis(self.retry_after_ms.unwrap_or(0)))
    }

    /// Whether a retry may resend the refused request unchanged. Synapse
    /// clears this when the request itself must change first.
    pub fn retry_same_request(&self) -> bool {
        self.safe_to_retry_same_request
    }
}

/// Record that a background retry for `pause` will not run before
/// `retry_at_ms`, so status shows when the wait really ends. A warm-up or
/// other transient refusal may carry no hint (or a hint shorter than the
/// worker's backoff); the daemon's own end time of an announced lane pause is
/// left as it is. Only the recorded pause the worker waits on is extended:
/// another root's identical pause, or a newer refusal that replaced this one,
/// keeps its own end time.
pub(crate) fn note_pause_retry_at(pause: &LanePause, retry_at_ms: u64) {
    if pause.cause == PauseCause::LanePaused || pause.record_id == 0 {
        return;
    }
    let mut pauses = lane_pauses()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(recorded) = pauses
        .values_mut()
        .find(|recorded| recorded.record_id == pause.record_id)
    {
        recorded.paused_until_ms = recorded.paused_until_ms.max(retry_at_ms);
    }
}

fn next_pause_record_id() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

fn lane_pauses() -> &'static Mutex<HashMap<PathBuf, LanePause>> {
    static PAUSES: OnceLock<Mutex<HashMap<PathBuf, LanePause>>> = OnceLock::new();
    PAUSES.get_or_init(Mutex::default)
}

/// Per project root and model, the most rows an `embed.batch` page may carry
/// after Synapse refused a page with `safe_to_retry_same_request: false`
/// (it does so for `deadline_exceeded`). Such a page must not be resent
/// unchanged, so the retry splits the same texts into pages half as large,
/// which also gives each page its own `request_key`. A one-row page cannot be
/// split further; it is sent again under a fresh `request_key` instead (see
/// [`refused_request_keys`]). Each fully embedded batch doubles
/// the limit back toward the model's recommended rows. It lives outside the
/// client because a background build opens a new client for every retry.
fn page_row_caps() -> &'static Mutex<HashMap<(PathBuf, String), usize>> {
    static CAPS: OnceLock<Mutex<HashMap<(PathBuf, String), usize>>> = OnceLock::new();
    CAPS.get_or_init(Mutex::default)
}

fn page_row_limit(root: &Path, model: &str, recommended: usize) -> usize {
    page_row_caps()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&(root.to_path_buf(), model.to_owned()))
        .map_or(recommended, |cap| recommended.min(*cap))
}

fn shrink_page_rows(root: &Path, model: &str, refused_rows: usize) {
    page_row_caps()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            (root.to_path_buf(), model.to_owned()),
            (refused_rows / 2).max(1),
        );
}

fn grow_page_rows(root: &Path, model: &str, recommended: usize) {
    let mut caps = page_row_caps()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let key = (root.to_path_buf(), model.to_owned());
    let Some(cap) = caps.get_mut(&key) else {
        return;
    };
    *cap = cap.saturating_mul(2);
    if *cap >= recommended {
        caps.remove(&key);
    }
}

/// One-row `embed.batch` pages Synapse refused with
/// `safe_to_retry_same_request: false`, by their stable request key, with how
/// often each was refused. Synapse's wire contract makes a request the "same"
/// one by its `request_key` and the digest bound to that key (operation,
/// model, constraints, ordered item ids and content hashes); a new key with
/// the same items is admitted as a new job. A row cannot be split the way a
/// larger refused page is, so its next send uses a fresh key derived from the
/// stable one and the refusal count instead: the same row, never the same
/// request. A successful send forgets the entry, so later sends of that row
/// use its normal key again: the stable key, or after any drop the key derived
/// from it and the drop count. [`RefusedRequestKeys`] bounds how many rows are
/// remembered and for how long; a row it drops before it succeeds is never
/// resent under a key that was refused.
fn refused_request_keys() -> &'static Mutex<RefusedRequestKeys> {
    static KEYS: OnceLock<Mutex<RefusedRequestKeys>> = OnceLock::new();
    KEYS.get_or_init(Mutex::default)
}

/// Most refused rows remembered at once. Only a successful send clears an
/// entry, so a refused row that is abandoned (its build is cancelled, its
/// root closes, its source changes) would otherwise stay for the whole
/// session; past this many, the entry refused longest ago is dropped first.
const MAX_REFUSED_REQUEST_KEYS: usize = 1_024;

/// How long a refused row is remembered after its last refusal.
const REFUSED_REQUEST_KEY_TTL_MS: u64 = 60 * 60 * 1_000;

#[derive(Default)]
struct RefusedRequestKeys {
    entries: HashMap<String, RefusedRequestKey>,
    /// How many entries the cap evicted, plus one for each expiry pass that
    /// removed any: drops other than a row succeeding. A dropped row's next
    /// send must not fall back to a key Synapse already refused, so every key
    /// sent after a drop is derived from this count too: each drop moves all
    /// rows onto keys never sent before it. Drops are rare, and a new key for
    /// the same items costs only a new Synapse job.
    drops: u64,
}

struct RefusedRequestKey {
    refusals: u64,
    last_refused_ms: u64,
}

impl RefusedRequestKeys {
    fn key_to_send(&mut self, stable_key: &str, now_ms: u64) -> String {
        self.expire(now_ms);
        let base = if self.drops == 0 {
            stable_key.to_owned()
        } else {
            hex_sha256(format!("{stable_key}:drops:{}", self.drops).as_bytes())
        };
        match self.entries.get(stable_key) {
            Some(entry) => hex_sha256(format!("{base}:resend:{}", entry.refusals).as_bytes()),
            None => base,
        }
    }

    fn note_refused(&mut self, stable_key: &str, now_ms: u64) {
        self.expire(now_ms);
        if !self.entries.contains_key(stable_key) && self.entries.len() >= MAX_REFUSED_REQUEST_KEYS
        {
            let oldest = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_refused_ms)
                .map(|(key, _)| key.clone());
            if let Some(oldest) = oldest {
                self.entries.remove(&oldest);
                self.drops += 1;
            }
        }
        let entry = self
            .entries
            .entry(stable_key.to_owned())
            .or_insert(RefusedRequestKey {
                refusals: 0,
                last_refused_ms: now_ms,
            });
        entry.refusals += 1;
        entry.last_refused_ms = now_ms;
    }

    fn forget(&mut self, stable_key: &str) {
        self.entries.remove(stable_key);
    }

    fn expire(&mut self, now_ms: u64) {
        let before = self.entries.len();
        self.entries.retain(|_, entry| {
            now_ms.saturating_sub(entry.last_refused_ms) < REFUSED_REQUEST_KEY_TTL_MS
        });
        if self.entries.len() < before {
            self.drops += 1;
        }
    }
}

/// Milliseconds since the first refused-row lookup in this process. The
/// refused-row ages use their own monotonic clock rather than the lane-pause
/// test clock, which is per test thread, so tests on other threads cannot age
/// out each other's entries.
fn refused_request_key_clock_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

fn request_key_to_send(stable_key: &str) -> String {
    refused_request_keys()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .key_to_send(stable_key, refused_request_key_clock_ms())
}

fn note_refused_request_key(stable_key: &str) {
    refused_request_keys()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .note_refused(stable_key, refused_request_key_clock_ms());
}

fn forget_refused_request_key(stable_key: &str) {
    refused_request_keys()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .forget(stable_key);
}

pub(crate) fn lane_pause_for_root(root: &Path) -> Option<LanePause> {
    let mut pauses = lane_pauses()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let pause = pauses.get(root)?.clone();
    if pause.paused_until_ms <= crate::semantic_index::unix_millis_now() {
        pauses.remove(root);
        return None;
    }
    Some(pause)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SynapseEmbeddingError {
    MissingConnectionFile,
    InvalidConnectionFile(String),
    MissingModel,
    DaemonUnavailable(String),
    CapabilityUnavailable(String),
    ModelUnavailable {
        requested: String,
        served: Vec<String>,
    },
    ModelNotCertified(String),
    LanePaused(LanePause),
    InvalidEnvelope(String),
    ContentHashMismatch {
        id: String,
        expected: String,
        actual: String,
    },
    FingerprintMismatch {
        expected: String,
        served: String,
    },
    TableEpochMismatch {
        expected: u64,
        served: u64,
    },
    CircuitOpen,
    Timeout(String),
    NoProgress(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SemanticBuildFailureClass {
    ModelLoading,
    Failure,
}

/// Classify the daemon's model-loading response after it crosses the existing
/// string-based semantic-index build boundary.
pub(crate) fn classify_semantic_build_failure(error: &str) -> SemanticBuildFailureClass {
    if error.starts_with("synapse_daemon_unavailable: model_loading:") {
        SemanticBuildFailureClass::ModelLoading
    } else {
        SemanticBuildFailureClass::Failure
    }
}

impl fmt::Display for SynapseEmbeddingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingConnectionFile => write!(
                formatter,
                "synapse_missing_connection_file: Synapse is daemon-only; run AFT under subc with Synapse registered and set user config subc.connection_file"
            ),
            Self::InvalidConnectionFile(path) => write!(
                formatter,
                "synapse_invalid_connection_file: subc.connection_file must be an absolute existing file: {path}"
            ),
            Self::MissingModel => write!(
                formatter,
                "synapse_missing_model: semantic.backend=synapse requires semantic.model"
            ),
            Self::DaemonUnavailable(error) => {
                write!(formatter, "synapse_daemon_unavailable: {error}")
            }
            Self::CapabilityUnavailable(error) => {
                write!(formatter, "synapse_capability_unavailable: {error}")
            }
            Self::ModelUnavailable { requested, served } => write!(
                formatter,
                "synapse_model_unavailable: requested {requested}; served models: {}",
                served.join(", ")
            ),
            Self::ModelNotCertified(model) => {
                write!(formatter, "synapse_model_not_certified: {model}")
            }
            Self::LanePaused(pause) => write!(
                formatter, "[transient] {LANE_PAUSE_MARKER}{}",
                serde_json::to_string(pause).map_err(|_| fmt::Error)?
            ),
            Self::InvalidEnvelope(error) => {
                write!(formatter, "synapse_invalid_envelope: {error}")
            }
            Self::ContentHashMismatch { id, expected, actual } => write!(
                formatter,
                "synapse_content_sha256_mismatch: item {id} expected {expected}, received {actual}"
            ),
            Self::FingerprintMismatch { expected, served } => write!(
                formatter,
                "synapse_fingerprint_mismatch: expected {expected}, served {served}"
            ),
            Self::TableEpochMismatch { expected, served } => write!(
                formatter,
                "synapse_table_epoch_mismatch: expected {expected}, served {served}"
            ),
            Self::CircuitOpen => write!(
                formatter,
                "synapse_circuit_open: repeated daemon timeouts temporarily paused embedding calls"
            ),
            Self::Timeout(operation) => {
                write!(formatter, "synapse_timeout: {operation} exceeded its deadline")
            }
            Self::NoProgress(job) => write!(
                formatter,
                "synapse_batch_no_progress: job {job} returned no new chunks repeatedly"
            ),
        }
    }
}

impl std::error::Error for SynapseEmbeddingError {}

#[cfg(test)]
pub(crate) mod pause_test_support {
    use super::*;
    use std::sync::{atomic::AtomicU64, Arc, Mutex};

    pub(crate) const MODEL: &str = "qwen3-embedding-0.6b-ane";
    pub(crate) const START: u64 = 100_000;
    pub(crate) const PAUSE_MS: u64 = 45 * 60 * 1_000;
    pub(crate) type Requests = Arc<Mutex<Vec<(u64, Value)>>>;

    /// Records an announced pause for `root` as a refused embedding request
    /// would, without a fake daemon.
    pub(crate) fn record_pause_for_root(root: &Path) {
        lane_pauses()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                root.to_path_buf(),
                LanePause {
                    lane_id: MODEL.into(),
                    fingerprint: Some("pinned".into()),
                    retry_after_ms: Some(PAUSE_MS),
                    paused_until_ms: START + PAUSE_MS,
                    reason: "scheduled maintenance".into(),
                    cause: Default::default(),
                    safe_to_retry_same_request: true,
                    record_id: 0,
                },
            );
    }

    /// Records, as a refused embedding request would, that the model of
    /// `root` is compiling a new input shape until `ready_at` on the test
    /// clock.
    pub(crate) fn record_warm_up_for_root(root: &Path, ready_at: u64) {
        lane_pauses()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                root.to_path_buf(),
                LanePause {
                    lane_id: MODEL.into(),
                    fingerprint: Some("pinned".into()),
                    retry_after_ms: Some(ready_at - START),
                    paused_until_ms: ready_at,
                    reason: "shape_compiling: compiling".into(),
                    cause: PauseCause::WarmingUp,
                    safe_to_retry_same_request: true,
                    record_id: 0,
                },
            );
    }

    pub(crate) fn pause_reply(retry_after_ms: Value) -> Value {
        json!({"error": {"code": "lane_paused", "transient": true,
            "retry_after_ms": retry_after_ms,
            "details": {"lane_id": MODEL, "fingerprint": "pinned",
                "paused_until_ms": START + PAUSE_MS, "reason": "scheduled maintenance"}}})
    }

    pub(crate) fn with_fake_synapse<T>(
        clock: Arc<AtomicU64>,
        retry_after_ms: Option<Value>,
        run: impl FnOnce(SemanticBackendConfig, Requests) -> T,
    ) -> T {
        with_fake_synapse_refusing(
            clock,
            START + PAUSE_MS,
            move |now| {
                pause_reply(
                    retry_after_ms
                        .clone()
                        .unwrap_or_else(|| json!(START + PAUSE_MS - now)),
                )
            },
            run,
        )
    }

    /// Synapse's reply while a request blocks on a cold shape compile, as
    /// the wire contract in the SYNAPSE repository
    /// (docs/wire-contract-v1.md, section 2) shows it: a transient refusal
    /// with a 1,000 ms retry hint.
    pub(crate) const SHAPE_COMPILING_REPLY: &str =
        include_str!("../tests/fixtures/synapse/error-shape-compiling-transient.json");

    /// Synapse's error object while a model lane is still loading.
    pub(crate) const MODEL_LOADING_ERROR: &str =
        include_str!("../tests/fixtures/synapse/error-model-loading-transient.json");

    /// A fake Synapse whose model lane needs `compile_ms` on the test clock
    /// to compile a cold input shape. Until then it answers every embedding
    /// request with the pinned `shape_compiling` fixture, its hint set to the
    /// time still left, and afterwards serves the same requests, as the real
    /// daemon does once its compile finishes. A one-second compile reproduces
    /// the fixture byte for byte on its first refusal.
    pub(crate) fn with_compiling_synapse<T>(
        clock: Arc<AtomicU64>,
        compile_ms: u64,
        run: impl FnOnce(SemanticBackendConfig, Requests) -> T,
    ) -> T {
        let reply: Value = serde_json::from_str(SHAPE_COMPILING_REPLY).unwrap();
        let ready_at = START + compile_ms;
        with_fake_synapse_refusing(
            clock,
            ready_at,
            move |now| {
                let mut reply = reply.clone();
                reply["result"]["error"]["retry_after_ms"] = json!(ready_at - now);
                reply
            },
            run,
        )
    }

    /// A fake Synapse that lists [`MODEL`] and refuses every embedding
    /// request with `refusal(now)` until the test clock reaches `ready_at`.
    pub(crate) fn with_fake_synapse_refusing<T>(
        clock: Arc<AtomicU64>,
        ready_at: u64,
        refusal: impl Fn(u64) -> Value + Send + 'static,
        run: impl FnOnce(SemanticBackendConfig, Requests) -> T,
    ) -> T {
        use subc_transport::connection_file::{self, ConnectionInfo, Endpoint, SCHEMA_VERSION};
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("connection.json");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let key = vec![0x42; subc_transport::KEY_LEN];
        let daemon_id = [0x24; subc_transport::DAEMON_ID_LEN];
        connection_file::write_atomic(
            &path,
            &ConnectionInfo {
                schema: SCHEMA_VERSION,
                wire_version: Some(subc_protocol::PROTOCOL_VERSION),
                endpoints: vec![Endpoint {
                    host: "127.0.0.1".into(),
                    port: listener.local_addr().unwrap().port(),
                }],
                key: key.clone(),
                daemon_id,
                pid: std::process::id(),
                daemon_ver: "synapse-test".into(),
            },
        )
        .unwrap();
        let requests = Requests::default();
        let recorded = requests.clone();
        let server = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async move {
                // Safety net only: stops a hung test from blocking forever. The
                // session lasts as long as the test's own work, which a loaded
                // CI runner (Windows) has stretched past 10 s, so keep it generous.
                tokio::time::timeout(Duration::from_secs(60), async move {
                    let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                    let (mut stream, _) = listener.accept().await.unwrap();
                    subc_transport::authenticate_server(&mut stream, &key, &daemon_id, "synapse-test", Duration::from_secs(2)).await.unwrap();
                    let catalog = subc_transport::read_frame(&mut stream).await.unwrap().unwrap();
                    respond(&mut stream, &catalog, json!({"op":"catalog.list","generation":1,"modules":[{
                        "module_id":"synapse","module_version":"1","roles":[{"role":"management_surface",
                        "operations":[{"name":"models.list","kind":"query"},{"name":"embed.query","kind":"query"},{"name":"embed.batch","kind":"query"}],
                        "config_schema":{"type":"object"},"observability":[],"identity_scope":[]}],"control_ops":[]}],"subc_ops":["catalog.list","route.open"]})).await;
                    let route = subc_transport::read_frame(&mut stream).await.unwrap().unwrap();
                    respond(&mut stream, &route, json!({"op":"route.open","route_channel":7,"route_epoch":1})).await;
                    while let Some(request) = subc_transport::read_frame(&mut stream).await.unwrap() {
                        let value: Value = serde_json::from_slice(&request.body).unwrap();
                        let now = clock.load(Ordering::SeqCst);
                        let mut requests = recorded.lock().unwrap();
                        assert!(requests.len() < 512, "embedding requests hot-looped during the pause");
                        requests.push((now, value.clone()));
                        drop(requests);
                        let params = &value["params"];
                        let reply = if value["method"] == MODELS_LIST_OPERATION {
                            json!({"models":[{"model":MODEL,"fingerprint":"pinned","table_epoch":1,"dims":2,
                                "recommended_batch":{"rows":100,"token_budget":8192}}]})
                        } else if now < ready_at {
                            refusal(now)
                        } else {
                            let items = params.get("items").and_then(Value::as_array).cloned().unwrap_or_else(|| vec![params.clone()]);
                            json!({"fingerprint":"pinned","table_epoch":1,"vectors":items.into_iter().map(|item|
                                json!({"id":item["id"],"content_sha256":item["content_sha256"],"vector":[1.0,0.0]})).collect::<Vec<_>>()})
                        };
                        respond(&mut stream, &request, reply).await;
                    }
                }).await.expect("fake Synapse watchdog");
            });
        });
        let config = SemanticBackendConfig {
            backend: crate::config::SemanticBackend::Synapse,
            model: MODEL.into(),
            subc_connection_file: Some(path),
            route_project_root: Some(directory.path().to_path_buf()),
            timeout_ms: 1_000,
            max_batch_size: 100,
            query_instruction: "off".into(),
            ..SemanticBackendConfig::default()
        };
        let result = run(config, requests);
        server.join().unwrap();
        result
    }

    async fn respond(
        stream: &mut tokio::net::TcpStream,
        request: &subc_protocol::Frame,
        value: Value,
    ) {
        use subc_protocol::{Flags, Frame, FrameType, Priority};
        let frame = Frame::build_with_version(
            request.header.ver,
            FrameType::Response,
            Flags::new(false, Priority::Interactive, false),
            request.header.channel,
            request.header.epoch,
            request.header.corr,
            serde_json::to_vec(&value).unwrap(),
        )
        .unwrap();
        subc_transport::write_frame(stream, &frame).await.unwrap();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SynapseIdentity {
    pub fingerprint: String,
    pub table_epoch: u64,
    pub equivalent_to: Vec<String>,
}

impl SynapseIdentity {
    pub fn accepts(&self, fingerprint: &str) -> bool {
        fingerprint == self.fingerprint
            || self.equivalent_to.iter().any(|alias| alias == fingerprint)
    }

    fn observe_equivalence(&mut self, served: &str, aliases: &[String]) -> bool {
        let connected = self.accepts(served)
            || aliases.iter().any(|alias| self.accepts(alias))
            || aliases.iter().any(|alias| alias == &self.fingerprint);
        if !connected {
            return false;
        }
        let mut class = HashSet::from([self.fingerprint.as_str(), served]);
        class.extend(self.equivalent_to.iter().map(String::as_str));
        class.extend(aliases.iter().map(String::as_str));
        self.equivalent_to = class
            .into_iter()
            .filter(|candidate| *candidate != self.fingerprint)
            .map(str::to_string)
            .collect();
        self.equivalent_to.sort();
        true
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SynapseModelMetadata {
    pub model: String,
    pub dims: Option<usize>,
    pub recommended_rows: usize,
    pub recommended_token_budget: usize,
    pub certified: bool,
    pub identity: SynapseIdentity,
}

#[derive(Debug, Clone)]
struct BatchItem {
    id: String,
    text: String,
    content_sha256: String,
}

struct SynapseState {
    connection_file: PathBuf,
    route_project_root: PathBuf,
    route_harness: String,
    model: String,
    call_timeout: Duration,
    required_operations: &'static [&'static str],
    connection: Arc<SynapseConnection>,
    consecutive_timeouts: usize,
    circuit_open_until: Option<Instant>,
}

struct ActiveConnection {
    consumer: SubcConsumer,
    route: RouteHandle,
}

fn connection_is_gone(error: &CallError) -> bool {
    // The SDK distinguishes lost routes/writers from request deadlines and
    // module replies. Bare NotSent errors also include flow-control timeouts,
    // so only a typed close reason proves that their route has ended.
    matches!(error, CallError::StaleRouteHandle(_))
        || error.close_reason().is_some()
        || matches!(
            error.outcome_cause(),
            Some(
                OutcomeUnknownCause::WriterClosed
                    | OutcomeUnknownCause::ConsumerClosed
                    | OutcomeUnknownCause::ConnectionFailed
                    | OutcomeUnknownCause::RouteEnded
            )
        )
}

#[derive(Hash, PartialEq, Eq)]
struct ConnectionKey {
    connection_file: PathBuf,
    harness: String,
    operations: &'static [&'static str],
}

struct SynapseConnection {
    active: tokio::sync::Mutex<Option<Arc<ActiveConnection>>>,
    runtime: Arc<SynapseRuntime>,
}

impl SynapseConnection {
    fn new() -> Result<Arc<Self>, SynapseEmbeddingError> {
        Ok(Arc::new(Self {
            active: tokio::sync::Mutex::new(None),
            runtime: Arc::new(SynapseRuntime::new()?),
        }))
    }

    fn shared(key: ConnectionKey) -> Result<Arc<Self>, SynapseEmbeddingError> {
        static CONNECTIONS: OnceLock<Mutex<HashMap<ConnectionKey, Weak<SynapseConnection>>>> =
            OnceLock::new();
        let mut connections = CONNECTIONS
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        connections.retain(|_, connection| connection.strong_count() != 0);
        if let Some(connection) = connections.get(&key).and_then(Weak::upgrade) {
            return Ok(connection);
        }
        let connection = Arc::new(Self {
            active: tokio::sync::Mutex::new(None),
            runtime: Arc::new(SynapseRuntime::shared()?),
        });
        connections.insert(key, Arc::downgrade(&connection));
        Ok(connection)
    }

    async fn reset(&self, failed: &Arc<ActiveConnection>) {
        let mut active = self.active.lock().await;
        if active
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, failed))
        {
            // Other in-flight callers may fail on this generation too. Only its
            // first failure closes it; late failures cannot close its replacement.
            failed.consumer.close().await;
            *active = None;
        }
    }
}

/// A synchronous client can be released by the async SubC frame loop, including
/// when its root's detached teardown thread has already released its reference.
/// Own shutdown here rather than relying on which caller drops the last Arc.
struct SynapseRuntime(Option<tokio::runtime::Runtime>);

impl SynapseRuntime {
    fn new() -> Result<Self, SynapseEmbeddingError> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map(|runtime| Self(Some(runtime)))
            .map_err(|error| SynapseEmbeddingError::DaemonUnavailable(error.to_string()))
    }

    fn shared() -> Result<Self, SynapseEmbeddingError> {
        // Transport tasks must keep running even when the model that opened
        // the connection is idle while another root is embedding.
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .map(|runtime| Self(Some(runtime)))
            .map_err(|error| SynapseEmbeddingError::DaemonUnavailable(error.to_string()))
    }

    fn block_on<F: std::future::Future>(&self, future: F) -> F::Output {
        self.0
            .as_ref()
            .expect("live Synapse runtime")
            .block_on(future)
    }
}

impl Drop for SynapseRuntime {
    fn drop(&mut self) {
        if let Some(runtime) = self.0.take() {
            // Cancel transport tasks without waiting for the blocking pool.
            // Tokio explicitly permits this shutdown from another async runtime;
            // DNS work already in progress can finish in the background.
            runtime.shutdown_background();
        }
    }
}

pub struct SynapseEmbeddingClient {
    runtime: Arc<SynapseRuntime>,
    state: SynapseState,
    metadata: SynapseModelMetadata,
    models_list_envelope: Vec<u8>,
}

impl SynapseEmbeddingClient {
    pub fn from_config(config: &SemanticBackendConfig) -> Result<Self, SynapseEmbeddingError> {
        let connection_file = config
            .subc_connection_file
            .clone()
            .ok_or(SynapseEmbeddingError::MissingConnectionFile)?;
        validate_connection_file(&connection_file)?;
        let model = config.model.trim();
        if model.is_empty() {
            return Err(SynapseEmbeddingError::MissingModel);
        }
        let route_harness = config.route_harness.clone().unwrap_or_else(|| "aft".into());
        let required_operations: &'static [&'static str] =
            &[MODELS_LIST_OPERATION, QUERY_OPERATION, BATCH_OPERATION];
        // This management route is unscoped: model and fingerprint travel in
        // each request, and pause bookkeeping stays on the calling root's state.
        // The bind root is only the first caller's diagnostic identity.
        let connection = SynapseConnection::shared(ConnectionKey {
            connection_file: std::fs::canonicalize(&connection_file)
                .unwrap_or_else(|_| connection_file.clone()),
            harness: route_harness.clone(),
            operations: required_operations,
        })?;
        let runtime = Arc::clone(&connection.runtime);
        let mut state = SynapseState {
            connection_file,
            route_project_root: config
                .route_project_root
                .clone()
                .or_else(|| std::env::current_dir().ok())
                .unwrap_or_else(|| PathBuf::from(".")),
            route_harness,
            model: model.to_string(),
            call_timeout: Duration::from_millis(config.timeout_ms.max(1)),
            required_operations,
            connection,
            consecutive_timeouts: 0,
            circuit_open_until: None,
        };
        let (metadata, models_list_envelope) = runtime.block_on(state.discover_model())?;
        Ok(Self {
            runtime,
            state,
            metadata,
            models_list_envelope,
        })
    }

    pub fn metadata(&self) -> &SynapseModelMetadata {
        &self.metadata
    }

    pub fn identity(&self) -> &SynapseIdentity {
        &self.metadata.identity
    }

    /// Raw successful discovery envelope, exposed for the gated live fixture probe.
    pub fn models_list_envelope(&self) -> &[u8] {
        &self.models_list_envelope
    }

    pub fn probe_dimension(&mut self, timeout: Duration) -> Result<usize, SynapseEmbeddingError> {
        let vector = self.embed_query("semantic index fingerprint probe", timeout)?;
        Ok(vector.len())
    }

    pub fn embed_query(
        &mut self,
        text: &str,
        timeout: Duration,
    ) -> Result<Vec<f32>, SynapseEmbeddingError> {
        let item = BatchItem {
            id: "query:0".to_string(),
            text: text.to_string(),
            content_sha256: content_sha256(text),
        };
        let params = constrained_params(
            &self.state.model,
            &self.metadata.identity,
            json!({
                "id": item.id,
                "text": item.text,
                "content_sha256": item.content_sha256,
                "deadline_ms": u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
            }),
        );
        let raw = self
            .runtime
            .block_on(self.state.call(QUERY_OPERATION, params, timeout))?;
        capture_live_envelope(QUERY_OPERATION, &raw);
        let parsed = parse_embedding_page(&raw, &[item.clone()])?;
        self.validate_page_identity(&parsed)?;
        let vector = parsed
            .vectors
            .get(&item.id)
            .or_else(|| parsed.vectors.values().next())
            .cloned()
            .ok_or_else(|| {
                SynapseEmbeddingError::InvalidEnvelope(
                    "embed.query response returned no vector".to_string(),
                )
            })?;
        self.validate_dimension(&vector)?;
        Ok(vector)
    }

    pub fn embed_batch(
        &mut self,
        texts: &[String],
    ) -> Result<Vec<Vec<f32>>, SynapseEmbeddingError> {
        let items = texts
            .iter()
            .enumerate()
            .map(|(index, text)| BatchItem {
                id: format!("item:{index}"),
                text: text.clone(),
                content_sha256: content_sha256(text),
            })
            .collect::<Vec<_>>();
        let pages = split_batch_pages(
            &items,
            page_row_limit(
                &self.state.route_project_root,
                &self.state.model,
                self.metadata.recommended_rows,
            ),
            self.metadata.recommended_token_budget,
        );
        let mut vectors = HashMap::with_capacity(items.len());
        for page in pages {
            match self.embed_batch_page(page) {
                Ok(page_vectors) => vectors.extend(page_vectors),
                Err(SynapseEmbeddingError::LanePaused(pause)) if !pause.retry_same_request() => {
                    shrink_page_rows(
                        &self.state.route_project_root,
                        &self.state.model,
                        page.len(),
                    );
                    return Err(SynapseEmbeddingError::LanePaused(pause));
                }
                Err(error) => return Err(error),
            }
        }
        grow_page_rows(
            &self.state.route_project_root,
            &self.state.model,
            self.metadata.recommended_rows,
        );
        items
            .iter()
            .map(|item| {
                vectors.remove(&item.id).ok_or_else(|| {
                    SynapseEmbeddingError::InvalidEnvelope(format!(
                        "embed.batch response omitted item {}",
                        item.id
                    ))
                })
            })
            .collect()
    }

    fn embed_batch_page(
        &mut self,
        items: &[BatchItem],
    ) -> Result<HashMap<String, Vec<f32>>, SynapseEmbeddingError> {
        let stable_key = batch_request_key(&self.state.model, &self.metadata.identity, items);
        let result = self.embed_batch_page_as(items, request_key_to_send(&stable_key));
        match &result {
            Ok(_) => forget_refused_request_key(&stable_key),
            Err(SynapseEmbeddingError::LanePaused(pause))
                if !pause.retry_same_request() && items.len() == 1 =>
            {
                note_refused_request_key(&stable_key);
            }
            Err(_) => {}
        }
        result
    }

    fn embed_batch_page_as(
        &mut self,
        items: &[BatchItem],
        request_key: String,
    ) -> Result<HashMap<String, Vec<f32>>, SynapseEmbeddingError> {
        let wire_items = items
            .iter()
            .map(|item| {
                json!({
                    "id": item.id,
                    "text": item.text,
                    "content_sha256": item.content_sha256,
                })
            })
            .collect::<Vec<_>>();
        let submit = constrained_params(
            &self.state.model,
            &self.metadata.identity,
            json!({ "items": wire_items, "request_key": request_key }),
        );
        let timeout = self.state.call_timeout;
        let deadline = Instant::now() + timeout;
        let raw = self
            .runtime
            .block_on(self.state.call(BATCH_OPERATION, submit, timeout))?;
        capture_live_envelope(BATCH_OPERATION, &raw);
        let mut page = parse_embedding_page(&raw, items)?;
        self.validate_page_identity(&page)?;
        let Some(job_id) = page.job_id.clone() else {
            self.validate_vectors(&page.vectors)?;
            return Ok(page.vectors);
        };

        let mut vectors = HashMap::new();
        vectors.extend(page.vectors.drain());
        let mut next_chunk = page.next_chunk_id.clone();
        let mut last_progress = (vectors.len(), next_chunk.clone());
        let mut no_progress_polls = 0usize;
        loop {
            if page.done && next_chunk.is_none() {
                break;
            }
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or_else(|| SynapseEmbeddingError::Timeout(BATCH_OPERATION.to_string()))?;
            let mut continuation = Map::new();
            continuation.insert("job_id".to_string(), Value::String(job_id.clone()));
            continuation.insert(
                "request_key".to_string(),
                Value::String(request_key.clone()),
            );
            if let Some(chunk_id) = next_chunk.clone() {
                continuation.insert("chunk_id".to_string(), Value::String(chunk_id));
            }
            let poll = constrained_params(
                &self.state.model,
                &self.metadata.identity,
                Value::Object(continuation),
            );
            let raw = self
                .runtime
                .block_on(self.state.call(BATCH_OPERATION, poll, remaining))?;
            capture_live_envelope(BATCH_OPERATION, &raw);
            page = parse_embedding_page(&raw, items)?;
            self.validate_page_identity(&page)?;
            vectors.extend(page.vectors.drain());
            next_chunk = page.next_chunk_id.clone();
            let progress = (vectors.len(), next_chunk.clone());
            if progress == last_progress {
                no_progress_polls += 1;
                if no_progress_polls >= MAX_NO_PROGRESS_POLLS {
                    return Err(SynapseEmbeddingError::NoProgress(job_id));
                }
                std::thread::sleep(
                    Duration::from_millis(100)
                        .min(deadline.saturating_duration_since(Instant::now())),
                );
            } else {
                no_progress_polls = 0;
                last_progress = progress;
            }
            if page.done && next_chunk.is_none() {
                break;
            }
        }
        self.validate_vectors(&vectors)?;
        Ok(vectors)
    }

    fn validate_page_identity(
        &mut self,
        page: &ParsedEmbeddingPage,
    ) -> Result<(), SynapseEmbeddingError> {
        if page.table_epoch != self.metadata.identity.table_epoch {
            return Err(SynapseEmbeddingError::TableEpochMismatch {
                expected: self.metadata.identity.table_epoch,
                served: page.table_epoch,
            });
        }
        if !self
            .metadata
            .identity
            .observe_equivalence(&page.fingerprint, &page.equivalent_to)
            || !self.metadata.identity.accepts(&page.fingerprint)
        {
            return Err(SynapseEmbeddingError::FingerprintMismatch {
                expected: self.metadata.identity.fingerprint.clone(),
                served: page.fingerprint.clone(),
            });
        }
        Ok(())
    }

    fn validate_vectors(
        &mut self,
        vectors: &HashMap<String, Vec<f32>>,
    ) -> Result<(), SynapseEmbeddingError> {
        for vector in vectors.values() {
            self.validate_dimension(vector)?;
        }
        Ok(())
    }

    fn validate_dimension(&mut self, vector: &[f32]) -> Result<(), SynapseEmbeddingError> {
        if vector.is_empty() || vector.iter().any(|value| !value.is_finite()) {
            return Err(SynapseEmbeddingError::InvalidEnvelope(
                "embedding vector must contain finite values".to_string(),
            ));
        }
        match self.metadata.dims {
            Some(dims) if dims != vector.len() => {
                Err(SynapseEmbeddingError::InvalidEnvelope(format!(
                    "embedding dimension mismatch: catalog={dims}, response={}",
                    vector.len()
                )))
            }
            None => {
                self.metadata.dims = Some(vector.len());
                Ok(())
            }
            Some(_) => Ok(()),
        }
    }
}

#[cfg(test)]
thread_local! {
    /// Test-only transport deadline for each request attempt on this thread,
    /// so a test can make every attempt of one call time out well inside the
    /// call's own budget without waiting for the SDK's default deadline.
    pub(crate) static ATTEMPT_TIMEOUT_FOR_TEST: std::cell::Cell<Option<Duration>> =
        const { std::cell::Cell::new(None) };
}

/// Options for one request attempt. Production uses the SDK defaults.
fn attempt_call_options() -> CallOptions {
    #[cfg(test)]
    if let Some(timeout) = ATTEMPT_TIMEOUT_FOR_TEST.with(std::cell::Cell::get) {
        return CallOptions {
            timeout,
            ..CallOptions::default()
        };
    }
    CallOptions::default()
}

fn typed_timeout_cause(error: &(dyn std::error::Error + 'static)) -> bool {
    let mut cause = Some(error);
    while let Some(error) = cause {
        if error
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.kind() == std::io::ErrorKind::TimedOut)
            || error.is::<tokio::time::error::Elapsed>()
        {
            return true;
        }
        cause = error.source();
    }
    false
}

fn classify_transport_failure(
    operation: &str,
    error: &(dyn std::error::Error + 'static),
) -> SynapseEmbeddingError {
    if typed_timeout_cause(error) {
        SynapseEmbeddingError::Timeout(operation.to_string())
    } else {
        SynapseEmbeddingError::DaemonUnavailable(error.to_string())
    }
}

fn classify_call_failure(
    operation: &str,
    error: &subc_client_rs::CallError,
) -> SynapseEmbeddingError {
    // Transport labels and diagnostics may contain "timeout" without a
    // timeout occurring. Inspect the typed cause, not its formatted wording.
    let timed_out = error.outcome_cause() == Some(OutcomeUnknownCause::Deadline)
        || match error {
            subc_client_rs::CallError::NotSent(cause)
            | subc_client_rs::CallError::OutcomeUnknown(cause) => {
                typed_timeout_cause(cause.as_ref())
            }
            _ => typed_timeout_cause(error),
        };
    if timed_out {
        SynapseEmbeddingError::Timeout(operation.to_string())
    } else {
        SynapseEmbeddingError::DaemonUnavailable(error.to_string())
    }
}

fn classify_attempt_failure(
    operation: &str,
    error: &CallError,
    deadline: tokio::time::Instant,
) -> SynapseEmbeddingError {
    // The SDK erases pre-send deadline causes into opaque NotSent errors.
    // Use the attempt's own deadline, not diagnostic wording, to recognize
    // expiry while waiting for route capacity or the writer.
    if matches!(error, CallError::NotSent(_)) && tokio::time::Instant::now() >= deadline {
        SynapseEmbeddingError::Timeout(operation.to_string())
    } else {
        classify_call_failure(operation, error)
    }
}

impl SynapseState {
    async fn discover_model(
        &mut self,
    ) -> Result<(SynapseModelMetadata, Vec<u8>), SynapseEmbeddingError> {
        let timeout = self.call_timeout.min(Duration::from_secs(3));
        let raw = self.call(MODELS_LIST_OPERATION, json!({}), timeout).await?;
        let models = parse_models_list(&raw)?;
        if let Some(model) = models.iter().find(|model| model.model == self.model) {
            if !model.certified {
                return Err(SynapseEmbeddingError::ModelNotCertified(self.model.clone()));
            }
            if model.recommended_rows == 0 || model.recommended_token_budget == 0 {
                return Err(SynapseEmbeddingError::InvalidEnvelope(format!(
                    "configured model {} has no usable recommended_batch rows/token_budget",
                    self.model
                )));
            }
            return Ok((model.clone(), raw));
        }
        let mut served = models
            .into_iter()
            .map(|model| model.model)
            .collect::<Vec<_>>();
        served.sort();
        Err(SynapseEmbeddingError::ModelUnavailable {
            requested: self.model.clone(),
            served,
        })
    }

    async fn call(
        &mut self,
        operation: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Vec<u8>, SynapseEmbeddingError> {
        // Routing, request retries and backoff share the caller's one budget.
        // A per-attempt timeout alone would restart it after each failure.
        let mut timeout_counted = false;
        let outcome = tokio::time::timeout(
            timeout,
            self.call_with_retries(operation, params, &mut timeout_counted),
        )
        .await;
        match outcome {
            Ok(result) => result,
            Err(_) => {
                // One logical call counts at most one timeout toward the
                // circuit, even when an attempt inside it already saw the
                // transport's own deadline. The shared route is not reset:
                // other callers may still be multiplexed on it.
                if !timeout_counted {
                    self.note_timeout();
                }
                if self.circuit_open_until.is_some() {
                    Err(SynapseEmbeddingError::CircuitOpen)
                } else {
                    Err(SynapseEmbeddingError::Timeout(operation.to_string()))
                }
            }
        }
    }

    /// Runs the attempts of one logical call. `timeout_counted` records
    /// whether this call has already counted its one timeout toward the
    /// circuit breaker, so later attempts and the caller's own deadline do
    /// not count it again.
    async fn call_with_retries(
        &mut self,
        operation: &str,
        params: Value,
        timeout_counted: &mut bool,
    ) -> Result<Vec<u8>, SynapseEmbeddingError> {
        if self
            .circuit_open_until
            .is_some_and(|until| Instant::now() < until)
        {
            return Err(SynapseEmbeddingError::CircuitOpen);
        }
        self.circuit_open_until = None;
        let body = serde_json::to_vec(&json!({ "method": operation, "params": params }))
            .map_err(|error| SynapseEmbeddingError::InvalidEnvelope(error.to_string()))?;
        let mut last_error = None;
        for attempt in 0..MAX_CALL_ATTEMPTS {
            match self.ensure_route().await {
                Err(error) => last_error = Some(error),
                Ok(connection) => {
                    // Do not hold the connection-slot lock across a request. The
                    // SDK multiplexes up to 1024 requests on a management route.
                    let options = attempt_call_options();
                    let deadline = tokio::time::Instant::now() + options.timeout;
                    let request =
                        connection
                            .consumer
                            .request(&connection.route, body.clone(), options);
                    match request.await {
                        Ok(response) => {
                            if response.len() > MAX_RESULT_PAGE_BYTES {
                                return Err(SynapseEmbeddingError::InvalidEnvelope(format!(
                                    "{operation} response exceeded 512KiB page bound"
                                )));
                            }
                            let mut result =
                                decode_result_envelope_for_request(response, Some(&params));
                            match &mut result {
                                Err(SynapseEmbeddingError::LanePaused(pause)) => {
                                    pause.record_id = next_pause_record_id();
                                    lane_pauses()
                                        .lock()
                                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                                        .insert(self.route_project_root.clone(), pause.clone());
                                }
                                _ => {
                                    self.consecutive_timeouts = 0;
                                    if operation != MODELS_LIST_OPERATION {
                                        lane_pauses()
                                            .lock()
                                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                                            .remove(&self.route_project_root);
                                    }
                                }
                            }
                            return result;
                        }
                        Err(error) => {
                            if matches!(error, CallError::Module(_)) {
                                return Err(SynapseEmbeddingError::DaemonUnavailable(
                                    error.to_string(),
                                ));
                            }
                            let failure = classify_attempt_failure(operation, &error, deadline);
                            if connection_is_gone(&error) {
                                self.connection.reset(&connection).await;
                            } else if matches!(failure, SynapseEmbeddingError::Timeout(_))
                                && !*timeout_counted
                            {
                                // Only this call's first attempt deadline
                                // counts. Its later attempts are the same slow
                                // call, so they must not open the circuit.
                                *timeout_counted = true;
                                self.note_timeout();
                                if self.circuit_open_until.is_some() {
                                    return Err(SynapseEmbeddingError::CircuitOpen);
                                }
                            }
                            last_error = Some(failure);
                        }
                    }
                }
            }
            if attempt + 1 < MAX_CALL_ATTEMPTS {
                tokio::time::sleep(Duration::from_millis(RETRY_BACKOFF_MS[attempt.min(2)])).await;
            }
        }
        Err(last_error.unwrap_or_else(|| SynapseEmbeddingError::DaemonUnavailable(String::new())))
    }

    async fn ensure_route(&self) -> Result<Arc<ActiveConnection>, SynapseEmbeddingError> {
        let mut active = self.connection.active.lock().await;
        if let Some(connection) = active.as_ref() {
            return Ok(Arc::clone(connection));
        }
        let options = ConsumerOptions {
            call_timeout: self.call_timeout,
            // AFT replaces the consumer and route together under the shared slot
            // lock. Disable SDK reconnects so a retired consumer cannot race that
            // replacement by authenticating a second, unused connection.
            reconnect_backoff: subc_client_rs::RetryBackoff {
                max_attempts: 0,
                ..Default::default()
            },
            ..ConsumerOptions::default()
        };
        let consumer = SubcConsumer::connect(&self.connection_file, options)
            .await
            .map_err(|error| classify_transport_failure("route setup", &error))?;
        let catalog = consumer
            .catalog_list()
            .await
            .map_err(|error| classify_call_failure("route setup", &error))?;
        if !catalog_advertises_operations(&catalog.modules, self.required_operations) {
            return Err(SynapseEmbeddingError::CapabilityUnavailable(format!(
                "Synapse is not registered with required operations: {}",
                self.required_operations.join(", ")
            )));
        }
        let route = consumer
            .open_route(
                RouteTarget::ManagementSurface {
                    module_id: SYNAPSE_MODULE_ID.to_string(),
                },
                BindIdentity::new(
                    self.route_project_root.to_string_lossy().into_owned(),
                    self.route_harness.clone(),
                    format!("aft-semantic-{}", std::process::id()),
                ),
                CallOptions {
                    consumer_identity: crate::launch_nonce::consumer_identity(),
                    ..CallOptions::default()
                },
            )
            .await
            .map_err(|error| classify_call_failure("route setup", &error))?;
        let connection = Arc::new(ActiveConnection { consumer, route });
        *active = Some(Arc::clone(&connection));
        Ok(connection)
    }

    fn note_timeout(&mut self) {
        self.consecutive_timeouts += 1;
        if self.consecutive_timeouts >= CIRCUIT_TIMEOUT_THRESHOLD {
            self.circuit_open_until = Some(Instant::now() + CIRCUIT_COOLDOWN);
        }
    }
}

fn capture_live_envelope(operation: &str, raw: &[u8]) {
    let Some(directory) = capture_directory_with(|name| std::env::var_os(name)) else {
        return;
    };
    let sequence = LIVE_CAPTURE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let operation = operation.replace('.', "-");
    let _ = std::fs::write(
        directory.join(format!("{operation}-{sequence}-live.json")),
        raw,
    );
}

fn capture_directory_with(
    lookup: impl FnOnce(&str) -> Option<std::ffi::OsString>,
) -> Option<PathBuf> {
    lookup("AFT_SYNAPSE_CAPTURE_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn validate_connection_file(path: &Path) -> Result<(), SynapseEmbeddingError> {
    if !path.is_absolute() || !path.is_file() {
        return Err(SynapseEmbeddingError::InvalidConnectionFile(
            path.to_string_lossy().into_owned(),
        ));
    }
    Ok(())
}

fn catalog_advertises_operations(
    entries: &[subc_client_rs::CatalogEntry],
    required_operations: &[&str],
) -> bool {
    entries.iter().any(|entry| {
        entry.module_id == SYNAPSE_MODULE_ID
            && entry.roles.iter().any(|role| {
                matches!(
                    role,
                    ProviderRole::ManagementSurface { operations, .. }
                        if required_operations.iter()
                            .all(|required| operations.iter().any(|operation| operation.name == *required))
                )
            })
    })
}

#[cfg(test)]
fn decode_result_envelope(response: Vec<u8>) -> Result<Vec<u8>, SynapseEmbeddingError> {
    decode_result_envelope_for_request(response, None)
}

fn decode_result_envelope_for_request(
    response: Vec<u8>,
    request: Option<&Value>,
) -> Result<Vec<u8>, SynapseEmbeddingError> {
    let value: Value = serde_json::from_slice(&response)
        .map_err(|error| SynapseEmbeddingError::InvalidEnvelope(error.to_string()))?;
    let result = value.get("result").cloned().unwrap_or(value);
    if let Some(error) = result.get("error") {
        let code = error
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("Synapse returned an unspecified error");
        return match code {
            "lane_paused" => {
                let details = &error["details"];
                let lane_id = details["lane_id"].as_str().unwrap_or_default();
                let fingerprint = details.get("fingerprint");
                if let Some(request) = request {
                    let model = request["model"].as_str().unwrap_or_default();
                    let pinned = request["required_fingerprint"].as_str().unwrap_or_default();
                    if lane_id != model
                        || fingerprint.is_some_and(|value| value.as_str() != Some(pinned))
                    {
                        return Err(SynapseEmbeddingError::FingerprintMismatch {
                            expected: format!("{model}:{pinned}"),
                            served: format!("{lane_id}:{}", fingerprint.unwrap_or(&Value::Null)),
                        });
                    }
                }
                let paused_until_ms = details["paused_until_ms"].as_u64().ok_or_else(|| {
                    SynapseEmbeddingError::InvalidEnvelope(
                        "lane_paused omitted paused_until_ms".into(),
                    )
                })?;
                let reason = details["reason"].as_str().ok_or_else(|| {
                    SynapseEmbeddingError::InvalidEnvelope("lane_paused omitted reason".into())
                })?;
                Err(SynapseEmbeddingError::LanePaused(LanePause {
                    lane_id: lane_id.to_owned(),
                    fingerprint: fingerprint.and_then(Value::as_str).map(str::to_owned),
                    retry_after_ms: error["retry_after_ms"]
                        .as_u64()
                        .filter(|ms| (1_000..=MAX_LANE_PAUSE_MS).contains(ms)),
                    paused_until_ms,
                    reason: reason.to_owned(),
                    cause: PauseCause::LanePaused,
                    safe_to_retry_same_request: error["safe_to_retry_same_request"]
                        .as_bool()
                        .unwrap_or(true),
                    record_id: 0,
                }))
            }
            "not_certified" => Err(SynapseEmbeddingError::ModelNotCertified(
                message.to_string(),
            )),
            "substitution_rejected" => Err(SynapseEmbeddingError::FingerprintMismatch {
                expected: "required fingerprint".to_string(),
                served: message.to_string(),
            }),
            // Transience is the reply's class, not its code: Synapse sends
            // `shape_compiling`, `model_loading`, `deadline_exceeded`,
            // `queue_full`, `engine_crashed` and quarantined lanes this way,
            // and may add codes. The two warm-up codes are named only so a
            // query can say the model is warming up.
            _ if error.get("class").and_then(Value::as_str) == Some("transient") => {
                Err(transient_refusal(code, message, error, request)
                    .map_or_else(|mismatch| mismatch, SynapseEmbeddingError::LanePaused))
            }
            _ => Err(SynapseEmbeddingError::DaemonUnavailable(format!(
                "{code}: {message}"
            ))),
        };
    }
    serde_json::to_vec(&result)
        .map_err(|error| SynapseEmbeddingError::InvalidEnvelope(error.to_string()))
}

/// Decode a reply of class `transient` into a pause that ends after its retry
/// hint. A missing, malformed or out-of-range hint (outside
/// [`MIN_TRANSIENT_RETRY_MS`]..=[`MAX_LANE_PAUSE_MS`]) is dropped, so the
/// waiting worker uses its normal backoff instead; it never makes the refusal
/// permanent. Both `details` fields are optional, but when the reply names
/// its lane (or that lane's fingerprint) it must be the requested model's, as
/// for `lane_paused`: another lane's refusal is a fingerprint mismatch, never
/// a pause of this lane.
fn transient_refusal(
    code: &str,
    message: &str,
    error: &Value,
    request: Option<&Value>,
) -> Result<LanePause, SynapseEmbeddingError> {
    let details = &error["details"];
    // Only embedding requests name a model and pinned fingerprint; a
    // `models.list` refusal has nothing to compare.
    if let Some(model) = request.and_then(|request| request["model"].as_str()) {
        let pinned = request
            .and_then(|request| request["required_fingerprint"].as_str())
            .unwrap_or_default();
        let lane_id = details.get("lane_id");
        let fingerprint = details.get("fingerprint");
        if lane_id.is_some_and(|value| value.as_str() != Some(model))
            || fingerprint.is_some_and(|value| value.as_str() != Some(pinned))
        {
            return Err(SynapseEmbeddingError::FingerprintMismatch {
                expected: format!("{model}:{pinned}"),
                served: format!(
                    "{}:{}",
                    lane_id.unwrap_or(&Value::Null),
                    fingerprint.unwrap_or(&Value::Null)
                ),
            });
        }
    }
    let retry_after_ms = error["retry_after_ms"]
        .as_u64()
        .filter(|ms| (MIN_TRANSIENT_RETRY_MS..=MAX_LANE_PAUSE_MS).contains(ms));
    let lane_id = details["lane_id"]
        .as_str()
        .or_else(|| request.and_then(|request| request["model"].as_str()))
        .unwrap_or_default();
    Ok(LanePause {
        lane_id: lane_id.to_owned(),
        fingerprint: request
            .and_then(|request| request["required_fingerprint"].as_str())
            .map(str::to_owned),
        retry_after_ms,
        paused_until_ms: crate::semantic_index::unix_millis_now()
            .saturating_add(retry_after_ms.unwrap_or(0)),
        reason: format!("{code}: {message}"),
        cause: if matches!(code, "shape_compiling" | "model_loading") {
            PauseCause::WarmingUp
        } else {
            PauseCause::Transient
        },
        safe_to_retry_same_request: error["safe_to_retry_same_request"]
            .as_bool()
            .unwrap_or(true),
        record_id: 0,
    })
}

fn parse_models_list(raw: &[u8]) -> Result<Vec<SynapseModelMetadata>, SynapseEmbeddingError> {
    let value: Value = serde_json::from_slice(raw)
        .map_err(|error| SynapseEmbeddingError::InvalidEnvelope(error.to_string()))?;
    let envelope = value.get("result").unwrap_or(&value);
    let table_epoch = integer_field(envelope, &["table_epoch", "tableEpoch"]);
    let entries = if let Some(entries) = envelope.get("models").and_then(Value::as_array) {
        entries
    } else if let Some(entries) = envelope.get("entries").and_then(Value::as_array) {
        entries
    } else if let Some(entries) = envelope.as_array() {
        entries
    } else {
        return Err(SynapseEmbeddingError::InvalidEnvelope(
            "models.list response has no models array".to_string(),
        ));
    };
    let mut models = Vec::with_capacity(entries.len());
    for entry in entries {
        let model = string_field(entry, &["model", "model_id"]).ok_or_else(|| {
            SynapseEmbeddingError::InvalidEnvelope("model entry has no id".to_string())
        })?;
        let certified = entry.get("certified").and_then(Value::as_bool) != Some(false)
            && string_field(entry, &["status", "state"]).as_deref() != Some("not_certified");
        let fingerprint = string_field(entry, &["fingerprint"])
            .or_else(|| {
                entry
                    .get("fingerprints")
                    .and_then(Value::as_array)
                    .and_then(|values| values.first())
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .ok_or_else(|| {
                SynapseEmbeddingError::InvalidEnvelope(format!("model {model} has no fingerprint"))
            })?;
        let model_epoch = integer_field(entry, &["table_epoch", "tableEpoch"])
            .or(table_epoch)
            .ok_or_else(|| {
                SynapseEmbeddingError::InvalidEnvelope(format!("model {model} has no table_epoch"))
            })?;
        let dims = integer_field(entry, &["dims", "dimensions"])
            .and_then(|value| usize::try_from(value).ok());
        let (recommended_rows, recommended_token_budget) = entry
            .get("recommended_batch")
            .or_else(|| entry.get("recommendedBatch"))
            .and_then(parse_recommended_batch)
            .unwrap_or((0, 0));
        models.push(SynapseModelMetadata {
            model,
            dims,
            recommended_rows,
            recommended_token_budget,
            certified,
            identity: SynapseIdentity {
                fingerprint,
                table_epoch: model_epoch,
                equivalent_to: string_array_field(entry, "equivalent_to"),
            },
        });
    }
    Ok(models)
}

fn parse_recommended_batch(value: &Value) -> Option<(usize, usize)> {
    let object = value.as_object()?;
    let rows = object
        .get("rows")?
        .as_u64()
        .and_then(|value| usize::try_from(value).ok())?;
    let token_budget = object
        .get("token_budget")
        .or_else(|| object.get("tokenBudget"))?
        .as_u64()
        .and_then(|value| usize::try_from(value).ok())?;
    (rows > 0 && token_budget > 0).then_some((rows, token_budget))
}

#[derive(Debug)]
struct ParsedEmbeddingPage {
    vectors: HashMap<String, Vec<f32>>,
    fingerprint: String,
    table_epoch: u64,
    equivalent_to: Vec<String>,
    job_id: Option<String>,
    next_chunk_id: Option<String>,
    done: bool,
}

fn parse_embedding_page(
    raw: &[u8],
    expected_items: &[BatchItem],
) -> Result<ParsedEmbeddingPage, SynapseEmbeddingError> {
    let value: Value = serde_json::from_slice(raw)
        .map_err(|error| SynapseEmbeddingError::InvalidEnvelope(error.to_string()))?;
    let envelope = value.get("result").unwrap_or(&value);
    let fingerprint =
        string_field(envelope, &["fingerprint", "served_fingerprint"]).ok_or_else(|| {
            SynapseEmbeddingError::InvalidEnvelope("response has no fingerprint".to_string())
        })?;
    let table_epoch = integer_field(envelope, &["table_epoch", "tableEpoch"]).ok_or_else(|| {
        SynapseEmbeddingError::InvalidEnvelope("response has no table_epoch".to_string())
    })?;
    let expected = expected_items
        .iter()
        .map(|item| (item.id.as_str(), item))
        .collect::<HashMap<_, _>>();
    let mut vectors = HashMap::new();
    if let Some(items) = envelope
        .get("vectors")
        .or_else(|| envelope.get("items"))
        .or_else(|| envelope.get("results"))
        .and_then(Value::as_array)
    {
        for (index, item) in items.iter().enumerate() {
            let id = string_field(item, &["id"])
                .or_else(|| expected_items.get(index).map(|item| item.id.clone()))
                .ok_or_else(|| {
                    SynapseEmbeddingError::InvalidEnvelope("vector item has no id".to_string())
                })?;
            let expected_item = expected.get(id.as_str()).ok_or_else(|| {
                SynapseEmbeddingError::InvalidEnvelope(format!("response returned unknown id {id}"))
            })?;
            verify_content_hash(item, expected_item)?;
            vectors.insert(id, parse_vector(item)?);
        }
    } else if envelope.get("vector").is_some() || envelope.get("embedding").is_some() {
        let expected_item = expected_items.first().ok_or_else(|| {
            SynapseEmbeddingError::InvalidEnvelope("unexpected unkeyed vector".to_string())
        })?;
        verify_content_hash(envelope, expected_item)?;
        vectors.insert(expected_item.id.clone(), parse_vector(envelope)?);
    }
    let next_chunk_id = string_field(
        envelope,
        &["next_chunk_id", "nextChunkId", "next_cursor", "cursor"],
    );
    let done = envelope.get("done").and_then(Value::as_bool) == Some(true)
        || envelope.get("complete").and_then(Value::as_bool) == Some(true)
        || (envelope.get("job_id").is_some() && next_chunk_id.is_none() && !vectors.is_empty());
    Ok(ParsedEmbeddingPage {
        vectors,
        fingerprint,
        table_epoch,
        equivalent_to: string_array_field(envelope, "equivalent_to"),
        job_id: string_field(envelope, &["job_id", "jobId"]),
        next_chunk_id,
        done,
    })
}

fn parse_vector(value: &Value) -> Result<Vec<f32>, SynapseEmbeddingError> {
    let values = value
        .get("vector")
        .or_else(|| value.get("embedding"))
        .and_then(Value::as_array)
        .ok_or_else(|| {
            SynapseEmbeddingError::InvalidEnvelope("vector item has no vector".to_string())
        })?;
    values
        .iter()
        .map(|value| {
            value
                .as_f64()
                .filter(|number| number.is_finite())
                .map(|number| number as f32)
                .ok_or_else(|| {
                    SynapseEmbeddingError::InvalidEnvelope(
                        "embedding vector contains a non-finite value".to_string(),
                    )
                })
        })
        .collect()
}

fn verify_content_hash(
    response: &Value,
    expected: &BatchItem,
) -> Result<(), SynapseEmbeddingError> {
    let actual = response
        .get("content_sha256")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            SynapseEmbeddingError::InvalidEnvelope(format!(
                "response item {} omitted content_sha256",
                expected.id
            ))
        })?;
    if actual != expected.content_sha256 {
        return Err(SynapseEmbeddingError::ContentHashMismatch {
            id: expected.id.clone(),
            expected: expected.content_sha256.clone(),
            actual: actual.to_string(),
        });
    }
    Ok(())
}

fn constrained_params(model: &str, identity: &SynapseIdentity, extra: Value) -> Value {
    let mut params = Map::new();
    params.insert("model".to_string(), Value::String(model.to_string()));
    params.insert(
        "required_fingerprint".to_string(),
        Value::String(identity.fingerprint.clone()),
    );
    params.insert("required_epoch".to_string(), json!(identity.table_epoch));
    params.insert("allow_equivalent".to_string(), Value::Bool(true));
    params.insert("accept_declared".to_string(), Value::Bool(false));
    if let Some(extra) = extra.as_object() {
        params.extend(extra.clone());
    }
    Value::Object(params)
}

fn batch_request_key(model: &str, identity: &SynapseIdentity, items: &[BatchItem]) -> String {
    let mut stable = BTreeMap::new();
    stable.insert("accept_declared", json!(false));
    stable.insert("allow_equivalent", json!(true));
    stable.insert(
        "content_sha256",
        json!(items
            .iter()
            .map(|item| &item.content_sha256)
            .collect::<Vec<_>>()),
    );
    stable.insert(
        "ids",
        json!(items.iter().map(|item| &item.id).collect::<Vec<_>>()),
    );
    stable.insert("model", json!(model));
    stable.insert("op", json!(BATCH_OPERATION));
    stable.insert("required_epoch", json!(identity.table_epoch));
    stable.insert("required_fingerprint", json!(identity.fingerprint));
    let encoded = serde_json::to_vec(&stable).expect("stable request-key value is serializable");
    hex_sha256(&encoded)
}

fn split_batch_pages(items: &[BatchItem], rows: usize, token_budget: usize) -> Vec<&[BatchItem]> {
    let mut pages = Vec::new();
    let mut page_start = 0usize;
    let mut page_tokens = 0usize;
    for (index, item) in items.iter().enumerate() {
        let item_tokens = item.text.chars().count().div_ceil(4).max(1);
        if index > page_start
            && (index - page_start >= rows
                || page_tokens.saturating_add(item_tokens) > token_budget)
        {
            pages.push(&items[page_start..index]);
            page_start = index;
            page_tokens = 0;
        }
        page_tokens = page_tokens.saturating_add(item_tokens);
    }
    if page_start < items.len() {
        pages.push(&items[page_start..]);
    }
    pages
}

fn string_field(value: &Value, names: &[&str]) -> Option<String> {
    names
        .iter()
        .find_map(|name| value.get(*name).and_then(Value::as_str))
        .map(str::to_string)
}

fn integer_field(value: &Value, names: &[&str]) -> Option<u64> {
    names
        .iter()
        .find_map(|name| value.get(*name).and_then(Value::as_u64))
}

fn string_array_field(value: &Value, name: &str) -> Vec<String> {
    value
        .get(name)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

fn content_sha256(text: &str) -> String {
    hex_sha256(text.as_bytes())
}

fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    #[tokio::test(start_paused = true)]
    async fn audit10_47_transport_label_does_not_turn_missing_route_into_timeout() {
        let directory = tempfile::tempdir().unwrap();
        let mut state = SynapseState {
            connection_file: directory.path().join("timeout-model/absent.json"),
            route_project_root: directory.path().to_path_buf(),
            route_harness: "aft".to_string(),
            model: "timeout-model".to_string(),
            call_timeout: Duration::from_secs(10),
            required_operations: &[],
            connection: SynapseConnection::new().unwrap(),
            consecutive_timeouts: 0,
            circuit_open_until: None,
        };
        let error = state
            .call_with_retries("embed.query", json!({}), &mut false)
            .await
            .unwrap_err();
        assert!(
            matches!(error, SynapseEmbeddingError::DaemonUnavailable(_)),
            "{error}"
        );
        assert_eq!(state.consecutive_timeouts, 0);
    }

    #[test]
    fn audit10_47_call_failure_uses_typed_timeout_cause() {
        let timeout = subc_client_rs::CallError::OutcomeUnknown(Box::new(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "deadline elapsed",
        )));
        assert!(matches!(
            classify_call_failure("embed.query", &timeout),
            SynapseEmbeddingError::Timeout(_)
        ));
        let transport = subc_client_rs::CallError::NotSent(Box::new(std::io::Error::other(
            "timeout-model is unavailable",
        )));
        assert!(matches!(
            classify_call_failure("embed.query", &transport),
            SynapseEmbeddingError::DaemonUnavailable(_)
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn synapse_presend_failure_before_deadline_stays_unavailable() {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        let error = CallError::NotSent(Box::new(std::io::Error::other(
            "timeout-model is unavailable",
        )));
        assert!(matches!(
            classify_attempt_failure(QUERY_OPERATION, &error, deadline),
            SynapseEmbeddingError::DaemonUnavailable(_)
        ));
    }

    #[test]
    fn model_loading_synapse_failure_is_classified_as_waiting_only_for_its_cause() {
        assert_eq!(
            super::classify_semantic_build_failure(
                "synapse_daemon_unavailable: model_loading: catalog lane 'qwen3-embedding-0.6b-ane' is loading"
            ),
            super::SemanticBuildFailureClass::ModelLoading
        );
        for error in [
            "synapse_daemon_unavailable: connection refused",
            "synapse_daemon_unavailable: module unavailable",
            "synapse_missing_model: model unset",
        ] {
            assert_eq!(
                super::classify_semantic_build_failure(error),
                super::SemanticBuildFailureClass::Failure,
                "{error} must remain a real failure"
            );
        }
    }

    #[test]
    fn lane_paused_identity_and_retry_bounds_are_validated() {
        use super::pause_test_support::*;
        let request = json!({"model": MODEL, "required_fingerprint": "pinned"});
        for hint in [
            json!(999),
            json!(MAX_LANE_PAUSE_MS + 1),
            json!(-1),
            json!("1000"),
            Value::Null,
        ] {
            let reply = pause_reply(hint);
            let error = decode_result_envelope_for_request(
                serde_json::to_vec(&reply).unwrap(),
                Some(&request),
            )
            .unwrap_err();
            let SynapseEmbeddingError::LanePaused(pause) = error else {
                panic!("{error}");
            };
            assert_eq!(pause.retry_after_ms, None);
            assert_eq!(
                pause.retry_delay(Duration::from_secs(15)),
                Duration::from_secs(15)
            );
        }
        for hint in [1_000, MAX_LANE_PAUSE_MS] {
            let reply = pause_reply(json!(hint));
            let error = decode_result_envelope_for_request(
                serde_json::to_vec(&reply).unwrap(),
                Some(&request),
            )
            .unwrap_err();
            assert_eq!(
                LanePause::from_error(&error.to_string())
                    .unwrap()
                    .retry_after_ms,
                Some(hint)
            );
        }
        for (field, value) in [
            ("lane_id", json!("another-lane")),
            ("fingerprint", json!("equivalent-but-not-pinned")),
            ("fingerprint", Value::Null),
        ] {
            let mut reply = pause_reply(json!(PAUSE_MS));
            reply["error"]["details"][field] = value;
            assert!(matches!(
                decode_result_envelope_for_request(
                    serde_json::to_vec(&reply).unwrap(),
                    Some(&request)
                ),
                Err(SynapseEmbeddingError::FingerprintMismatch { .. })
            ));
        }
        let mut legacy = pause_reply(json!(PAUSE_MS));
        legacy["error"]["details"]
            .as_object_mut()
            .unwrap()
            .remove("fingerprint");
        assert!(matches!(
            decode_result_envelope_for_request(
                serde_json::to_vec(&legacy).unwrap(),
                Some(&request)
            ),
            Err(SynapseEmbeddingError::LanePaused(_))
        ));
        assert!(matches!(
            decode_result_envelope(
                br#"{"error":{"code":"future_code","transient":true}}"#.to_vec()
            ),
            Err(SynapseEmbeddingError::DaemonUnavailable(_))
        ));
    }

    #[test]
    fn lane_paused_fake_synapse_preserves_breaker_and_reports_health() {
        use super::pause_test_support::*;
        crate::semantic_index::with_lane_pause_clock(START, |clock| {
            with_fake_synapse(clock.clone(), None, |config, requests| {
                let mut client = SynapseEmbeddingClient::from_config(&config).unwrap();
                client.state.consecutive_timeouts = 2;
                let error = client
                    .embed_query("query", Duration::from_secs(1))
                    .unwrap_err();
                assert!(matches!(error, SynapseEmbeddingError::LanePaused(_)));
                assert_eq!(client.state.consecutive_timeouts, 2);
                assert!(client.state.circuit_open_until.is_none());
                assert_eq!(clock.load(Ordering::SeqCst), START);
                let root = config.route_project_root.as_deref().unwrap();
                let pause = lane_pause_for_root(root).unwrap();
                assert_eq!(pause.paused_until_ms, START + PAUSE_MS);
                assert_eq!(pause.reason, "scheduled maintenance");
                clock.store(START + PAUSE_MS, Ordering::SeqCst);
                assert!(lane_pause_for_root(root).is_none());
                assert_eq!(
                    client.embed_query("query", Duration::from_secs(1)).unwrap(),
                    vec![1.0, 0.0]
                );
                assert_eq!(client.state.consecutive_timeouts, 0);
                assert_eq!(requests.lock().unwrap().len(), 3);
            });
        });
    }

    #[test]
    fn lane_paused_envelope_is_transient() {
        let response = serde_json::to_vec(&json!({"error": {
            "code": "lane_paused", "transient": true, "retry_after_ms": 2_700_000,
            "details": {"lane_id": "qwen3-embedding-0.6b-ane", "fingerprint": "pinned",
                "paused_until_ms": 2_800_000, "reason": "scheduled maintenance"}
        }}))
        .unwrap();
        let error = decode_result_envelope(response).unwrap_err();
        assert!(error.to_string().starts_with("[transient] "), "{error}");
    }

    fn decode_refusal(reply: &Value) -> SynapseEmbeddingError {
        use super::pause_test_support::MODEL;
        let request = json!({"model": MODEL, "required_fingerprint": "pinned"});
        decode_result_envelope_for_request(serde_json::to_vec(reply).unwrap(), Some(&request))
            .unwrap_err()
    }

    fn decoded_pause(reply: &Value) -> LanePause {
        let error = decode_refusal(reply);
        let rendered = error.to_string();
        assert!(rendered.starts_with("[transient] "), "{rendered}");
        let SynapseEmbeddingError::LanePaused(pause) = error else {
            panic!("{rendered}");
        };
        assert_eq!(LanePause::from_error(&rendered).as_ref(), Some(&pause));
        pause
    }

    #[test]
    fn shape_compiling_fixture_decodes_as_a_warm_up_with_its_hint() {
        use super::pause_test_support::*;
        crate::semantic_index::with_lane_pause_clock(START, |_| {
            let pause = decoded_pause(&serde_json::from_str(SHAPE_COMPILING_REPLY).unwrap());
            assert_eq!(pause.cause, PauseCause::WarmingUp);
            assert_eq!(pause.retry_after_ms, Some(1_000));
            assert_eq!(pause.paused_until_ms, START + 1_000);
            assert_eq!(pause.lane_id, MODEL);
            assert!(pause.retry_same_request());
            assert_eq!(pause.reason, "shape_compiling: …");
            assert_eq!(
                pause.retry_delay(Duration::from_millis(100)),
                Duration::from_secs(1)
            );
        });
    }

    #[test]
    fn model_loading_fixture_decodes_as_a_warm_up_with_its_250ms_hint() {
        use super::pause_test_support::*;
        crate::semantic_index::with_lane_pause_clock(START, |_| {
            let error: Value = serde_json::from_str(MODEL_LOADING_ERROR).unwrap();
            // The fixture has no `details`; the lane comes from the request.
            let pause = decoded_pause(&json!({"result": {"error": error}}));
            assert_eq!(pause.cause, PauseCause::WarmingUp);
            assert_eq!(pause.retry_after_ms, Some(250));
            assert_eq!(pause.paused_until_ms, START + 250);
            assert_eq!(pause.lane_id, MODEL);
        });
    }

    #[test]
    fn any_transient_class_reply_is_a_pause_carrying_its_hint() {
        for (code, hint, safe, details) in [
            ("queue_full", 2_000, true, Value::Null),
            ("deadline_exceeded", 100, false, Value::Null),
            ("engine_crashed", 5_000, true, Value::Null),
            (
                "lane_quarantined",
                600_000,
                true,
                json!({"lane_id": "qwen3-embedding-0.6b-ane", "quarantined": true}),
            ),
            ("some_future_code", 30_000, true, Value::Null),
        ] {
            let pause = decoded_pause(&json!({"result": {"error": {
                "class": "transient", "code": code, "message": "try later",
                "retry_after_ms": hint, "safe_to_retry_same_request": safe,
                "details": details,
            }}}));
            assert_eq!(pause.cause, PauseCause::Transient, "{code}");
            assert_eq!(pause.retry_after_ms, Some(hint), "{code}");
            assert_eq!(pause.retry_same_request(), safe, "{code}");
            assert_eq!(pause.reason, format!("{code}: try later"));
        }
    }

    #[test]
    fn transient_hint_out_of_range_or_malformed_uses_the_normal_backoff() {
        for hint in [
            json!(MIN_TRANSIENT_RETRY_MS - 1),
            json!(MAX_LANE_PAUSE_MS + 1),
            json!(-1),
            json!(1.5),
            json!("1000"),
            Value::Null,
        ] {
            let pause = decoded_pause(&json!({"result": {"error": {
                "class": "transient", "code": "shape_compiling", "message": "compiling",
                "retry_after_ms": hint, "safe_to_retry_same_request": true,
            }}}));
            assert_eq!(pause.cause, PauseCause::WarmingUp, "{hint}");
            assert_eq!(pause.retry_after_ms, None, "{hint}");
            assert_eq!(
                pause.retry_delay(Duration::from_secs(15)),
                Duration::from_secs(15),
                "{hint}"
            );
        }
        for hint in [MIN_TRANSIENT_RETRY_MS, MAX_LANE_PAUSE_MS] {
            let pause = decoded_pause(&json!({"result": {"error": {
                "class": "transient", "code": "queue_full", "retry_after_ms": hint,
            }}}));
            assert_eq!(pause.retry_after_ms, Some(hint));
        }
    }

    #[test]
    fn transient_reply_for_another_lane_does_not_pause_this_one() {
        use super::pause_test_support::*;
        let mut reply: Value = serde_json::from_str(SHAPE_COMPILING_REPLY).unwrap();
        reply["result"]["error"]["details"]["lane_id"] = json!("another-lane");
        assert!(matches!(
            decode_refusal(&reply),
            SynapseEmbeddingError::FingerprintMismatch { .. }
        ));
        let mut pinned_elsewhere = reply.clone();
        pinned_elsewhere["result"]["error"]["details"] = json!({
            "lane_id": MODEL,
            "fingerprint": "another-fingerprint",
        });
        assert!(matches!(
            decode_refusal(&pinned_elsewhere),
            SynapseEmbeddingError::FingerprintMismatch { .. }
        ));
        // Through the client, the other lane's refusal records no pause.
        crate::semantic_index::with_lane_pause_clock(START, |clock| {
            with_fake_synapse_refusing(
                clock,
                START + 1,
                move |_| reply.clone(),
                |config, _| {
                    let mut client = SynapseEmbeddingClient::from_config(&config).unwrap();
                    assert!(matches!(
                        client.embed_query("query", Duration::from_secs(1)),
                        Err(SynapseEmbeddingError::FingerprintMismatch { .. })
                    ));
                    assert!(
                        lane_pause_for_root(config.route_project_root.as_deref().unwrap())
                            .is_none()
                    );
                },
            );
        });
    }

    #[test]
    fn transient_reply_without_a_hint_is_still_a_pause() {
        let pause = decoded_pause(&json!({"result": {"error": {
            "class": "transient", "code": "shape_compiling", "message": "compiling",
        }}}));
        assert_eq!(pause.cause, PauseCause::WarmingUp);
        assert_eq!(pause.retry_after_ms, None);
        assert!(pause.retry_same_request());
    }

    #[test]
    fn permanent_or_unclassed_reply_is_not_a_pause() {
        for error in [
            json!({"class": "permanent", "code": "invalid_request", "message": "bad",
                "retry_after_ms": 1_000, "safe_to_retry_same_request": false}),
            json!({"code": "shape_compiling", "message": "no class"}),
        ] {
            assert!(matches!(
                decode_refusal(&json!({"result": {"error": error}})),
                SynapseEmbeddingError::DaemonUnavailable(_)
            ));
        }
    }

    #[test]
    fn shape_compiling_query_fails_at_once_without_tripping_the_breaker() {
        use super::pause_test_support::*;
        crate::semantic_index::with_lane_pause_clock(START, |clock| {
            with_compiling_synapse(clock.clone(), 90_000, |config, requests| {
                let mut client = SynapseEmbeddingClient::from_config(&config).unwrap();
                client.state.consecutive_timeouts = 2;
                let error = client
                    .embed_query("query", Duration::from_secs(1))
                    .unwrap_err();
                let SynapseEmbeddingError::LanePaused(pause) = &error else {
                    panic!("{error}");
                };
                assert_eq!(pause.cause, PauseCause::WarmingUp);
                assert_eq!(pause.retry_after_ms, Some(90_000));
                assert_eq!(client.state.consecutive_timeouts, 2);
                assert!(client.state.circuit_open_until.is_none());
                assert_eq!(clock.load(Ordering::SeqCst), START);
                let root = config.route_project_root.as_deref().unwrap();
                let recorded = lane_pause_for_root(root).unwrap();
                assert_eq!(recorded.paused_until_ms, START + 90_000);
                assert_eq!(recorded.cause, PauseCause::WarmingUp);
                clock.store(START + 90_000, Ordering::SeqCst);
                assert_eq!(
                    client.embed_query("query", Duration::from_secs(1)).unwrap(),
                    vec![1.0, 0.0]
                );
                assert!(lane_pause_for_root(root).is_none());
                assert_eq!(requests.lock().unwrap().len(), 3);
            });
        });
    }

    /// Models one send of `row` at `now_ms` that Synapse refuses as unsafe,
    /// and returns the key that send used.
    fn refuse_row(keys: &mut RefusedRequestKeys, row: &str, now_ms: u64) -> String {
        let sent = keys.key_to_send(row, now_ms);
        keys.note_refused(row, now_ms);
        sent
    }

    #[test]
    fn refused_rows_past_the_cap_drop_oldest_first_and_never_reuse_a_refused_key() {
        let mut keys = RefusedRequestKeys::default();
        let mut refused = vec![refuse_row(&mut keys, "row-0", 0)];
        // Refused a second time, so both its stable and its first resend key
        // were refused.
        refused.push(refuse_row(&mut keys, "row-0", 1));
        for row in 1..=MAX_REFUSED_REQUEST_KEYS {
            refused.push(refuse_row(&mut keys, &format!("row-{row}"), 1 + row as u64));
        }
        assert_eq!(keys.entries.len(), MAX_REFUSED_REQUEST_KEYS);
        assert!(
            !keys.entries.contains_key("row-0"),
            "the oldest entry stayed"
        );
        assert!(keys.entries.contains_key("row-1"));
        let now = 2 + MAX_REFUSED_REQUEST_KEYS as u64;
        let dropped = keys.key_to_send("row-0", now);
        assert!(
            !refused.contains(&dropped),
            "a row dropped by the cap was resent under a key Synapse refused"
        );
        assert!(!refused.contains(&keys.key_to_send("row-1", now)));
    }

    #[test]
    fn refused_rows_expire_after_their_age_limit_and_never_reuse_a_refused_key() {
        let mut keys = RefusedRequestKeys::default();
        let refused = refuse_row(&mut keys, "row", 0);
        let resend = keys.key_to_send("row", REFUSED_REQUEST_KEY_TTL_MS - 1);
        assert_ne!(resend, refused);
        assert!(
            keys.entries.contains_key("row"),
            "expired before its age limit"
        );
        let late = keys.key_to_send("row", REFUSED_REQUEST_KEY_TTL_MS);
        assert!(keys.entries.is_empty(), "kept past its age limit");
        assert_ne!(
            late, refused,
            "an expired row was resent under the key Synapse refused"
        );
        // Without any drop the stable key is used as before.
        assert_eq!(RefusedRequestKeys::default().key_to_send("row", 0), "row");
    }

    #[test]
    fn unsafe_refusal_of_one_row_resends_it_as_a_new_request() {
        use super::pause_test_support::*;
        crate::semantic_index::with_lane_pause_clock(START, |clock| {
            let refusal = json!({"result": {"error": {
                "class": "transient", "code": "deadline_exceeded", "message": "deadline",
                "retry_after_ms": 100, "safe_to_retry_same_request": false,
            }}});
            with_fake_synapse_refusing(
                clock.clone(),
                START + 100,
                move |_| refusal.clone(),
                |config, requests| {
                    // A text no other test embeds, so its stable key is ours.
                    let texts = vec!["one row refused with an unsafe deadline".to_string()];
                    let mut client = SynapseEmbeddingClient::from_config(&config).unwrap();
                    assert!(matches!(
                        client.embed_batch(&texts),
                        Err(SynapseEmbeddingError::LanePaused(_))
                    ));
                    clock.store(START + 100, Ordering::SeqCst);
                    let mut client = SynapseEmbeddingClient::from_config(&config).unwrap();
                    assert_eq!(client.embed_batch(&texts).unwrap().len(), 1);
                    assert_eq!(client.embed_batch(&texts).unwrap().len(), 1);
                    let sent = requests
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|(_, request)| request["method"] == BATCH_OPERATION)
                        .map(|(_, request)| {
                            (
                                request["params"]["request_key"]
                                    .as_str()
                                    .unwrap()
                                    .to_owned(),
                                request["params"]["items"].clone(),
                            )
                        })
                        .collect::<Vec<_>>();
                    assert_eq!(sent.len(), 3);
                    assert!(sent.iter().all(|(_, items)| *items == sent[0].1));
                    assert_ne!(
                        sent[1].0, sent[0].0,
                        "the refused row was resent as the same request"
                    );
                    assert_eq!(
                        sent[2].0, sent[0].0,
                        "after it succeeded the row did not return to its stable key"
                    );
                },
            );
        });
    }

    #[test]
    fn unsafe_transient_refusal_splits_the_page_before_it_is_sent_again() {
        use super::pause_test_support::*;
        crate::semantic_index::with_lane_pause_clock(START, |clock| {
            let refusal = json!({"result": {"error": {
                "class": "transient", "code": "deadline_exceeded", "message": "deadline",
                "retry_after_ms": 100, "safe_to_retry_same_request": false,
            }}});
            with_fake_synapse_refusing(
                clock.clone(),
                START + 100,
                move |_| refusal.clone(),
                |config, requests| {
                    let texts = ["a", "b", "c", "d"].map(String::from).to_vec();
                    let mut client = SynapseEmbeddingClient::from_config(&config).unwrap();
                    assert!(matches!(
                        client.embed_batch(&texts),
                        Err(SynapseEmbeddingError::LanePaused(_))
                    ));
                    clock.store(START + 100, Ordering::SeqCst);
                    // A background build opens a new client for its retry.
                    let mut client = SynapseEmbeddingClient::from_config(&config).unwrap();
                    assert_eq!(client.embed_batch(&texts).unwrap().len(), 4);
                    let pages = requests
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|(_, request)| request["method"] == BATCH_OPERATION)
                        .map(|(_, request)| request["params"]["items"].as_array().unwrap().len())
                        .collect::<Vec<_>>();
                    assert_eq!(pages, vec![4, 2, 2]);
                },
            );
        });
    }

    #[test]
    fn audit10_batch_pages_borrow_original_items() {
        let items = vec![item("a", "first"), item("b", "second"), item("c", "third")];
        let pages = super::split_batch_pages(&items, 2, usize::MAX);
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0].len(), 2);
        assert_eq!(pages[1].len(), 1);
        assert_eq!(pages[0].as_ptr(), items.as_ptr());
        assert_eq!(pages[1].as_ptr(), items[2..].as_ptr());
        let pages = super::split_batch_pages(&items, 10, 2);
        assert_eq!(
            pages.iter().map(|p| p.len()).collect::<Vec<_>>(),
            vec![1, 1, 1]
        );
        assert!(super::split_batch_pages(&[], 10, 10).is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn audit10_synapse_deadline_includes_route_and_retry_work() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = super::SynapseState {
            connection_file: dir.path().join("absent.json"),
            route_project_root: dir.path().to_path_buf(),
            route_harness: "aft".to_string(),
            model: "test".to_string(),
            call_timeout: std::time::Duration::from_millis(1),
            required_operations: &[],
            connection: super::SynapseConnection::new().unwrap(),
            consecutive_timeouts: 0,
            circuit_open_until: None,
        };
        let error = state
            .call("embed.query", serde_json::json!({}), state.call_timeout)
            .await
            .unwrap_err();
        assert!(
            matches!(error, super::SynapseEmbeddingError::Timeout(_)),
            "{error}"
        );
        assert_eq!(state.consecutive_timeouts, 1);
        assert!(state.connection.active.lock().await.is_none());
    }

    use super::*;

    #[test]
    fn embedding_client_can_drop_on_async_frame_loop() {
        let connection = tempfile::NamedTempFile::new().unwrap();
        let config = SemanticBackendConfig {
            subc_connection_file: Some(connection.path().to_path_buf()),
            ..Default::default()
        };
        // Discovery is irrelevant to destruction. Reuse the production
        // constructor's real runtime and state, with recorded model metadata.
        let transport = SynapseRerankTransport::new(&config).unwrap();
        let envelope = include_bytes!("../tests/fixtures/synapse/models-list-live-raw.json");
        let metadata = parse_models_list(envelope).unwrap().remove(0);
        let client = SynapseEmbeddingClient {
            runtime: transport.runtime,
            state: transport.state,
            metadata,
            models_list_envelope: envelope.to_vec(),
        };
        let outer = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        outer.block_on(async move { drop(client) });
    }

    #[test]
    fn synapse_runtime_shutdown_cancels_transport_tasks() {
        struct NotifyDrop(std::sync::mpsc::Sender<()>);
        impl Drop for NotifyDrop {
            fn drop(&mut self) {
                self.0.send(()).unwrap();
            }
        }
        let inner = SynapseRuntime::new().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let task = inner.block_on(async {
            let guard = NotifyDrop(tx);
            let task = tokio::spawn(async move {
                let _guard = guard;
                std::future::pending::<()>().await;
            });
            tokio::task::yield_now().await;
            task
        });
        let outer = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        outer.block_on(async move { drop(inner) });
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(outer.block_on(task).unwrap_err().is_cancelled());
    }

    #[test]
    fn empty_synapse_capture_directory_is_unset_with_an_injected_lookup() {
        assert_eq!(
            capture_directory_with(|key| {
                assert_eq!(key, "AFT_SYNAPSE_CAPTURE_DIR");
                Some(std::ffi::OsString::new())
            }),
            None
        );
        assert_eq!(
            capture_directory_with(|_| Some(std::ffi::OsString::from("/captures"))),
            Some(PathBuf::from("/captures"))
        );
    }

    fn item(id: &str, text: &str) -> BatchItem {
        BatchItem {
            id: id.to_string(),
            text: text.to_string(),
            content_sha256: content_sha256(text),
        }
    }

    #[test]
    fn models_list_live_shapes_allow_optional_dims() {
        let models = parse_models_list(include_bytes!(
            "../tests/fixtures/synapse/models-list-live-raw.json"
        ))
        .unwrap();
        assert_eq!(models.len(), 7);
        let selected = models
            .iter()
            .find(|model| model.model == "gte-modernbert-base-ane-fp16")
            .unwrap();
        assert_eq!(selected.dims, None);
        assert_eq!(selected.recommended_rows, 8);
        assert_eq!(selected.recommended_token_budget, 4096);
    }

    #[test]
    fn captured_live_error_envelope_is_typed() {
        let raw = include_bytes!("../tests/fixtures/synapse/embed-query-0-live.json").to_vec();
        let error = decode_result_envelope(raw).unwrap_err();
        assert!(matches!(error, SynapseEmbeddingError::ModelNotCertified(_)));
    }

    #[test]
    fn content_hash_mismatch_is_loud_and_typed() {
        let expected = item("item:0", "hello");
        let raw = br#"{
          "fingerprint":"fp-a","table_epoch":7,
          "vectors":[{"id":"item:0","content_sha256":"wrong","vector":[1.0]}]
        }"#;
        let error = parse_embedding_page(raw, &[expected]).unwrap_err();
        assert!(matches!(
            error,
            SynapseEmbeddingError::ContentHashMismatch { .. }
        ));
        assert!(error
            .to_string()
            .contains("synapse_content_sha256_mismatch"));
    }

    #[test]
    fn fingerprint_equivalence_class_accepts_aliases() {
        let mut identity = SynapseIdentity {
            fingerprint: "fp-current".to_string(),
            table_epoch: 7,
            equivalent_to: vec!["fp-old".to_string()],
        };
        assert!(identity.accepts("fp-old"));
        assert!(identity.observe_equivalence("fp-new", &["fp-current".to_string()]));
        assert!(identity.accepts("fp-new"));
        assert!(!identity.accepts("foreign"));
        assert!(!identity.observe_equivalence("foreign", &[]));
    }

    #[test]
    fn pages_reassemble_by_chunk_and_item_id() {
        let expected = vec![item("item:0", "a"), item("item:1", "b")];
        let first = parse_embedding_page(
            include_bytes!("../tests/fixtures/synapse/embed-batch-page-1.json"),
            &expected,
        )
        .unwrap();
        let second = parse_embedding_page(
            include_bytes!("../tests/fixtures/synapse/embed-batch-page-2.json"),
            &expected,
        )
        .unwrap();
        assert_eq!(first.next_chunk_id.as_deref(), Some("chunk-2"));
        let mut vectors = first.vectors;
        vectors.extend(second.vectors);
        assert_eq!(vectors.len(), 2);
        assert_eq!(vectors["item:1"], vec![0.0, 1.0]);
    }

    #[test]
    fn request_key_is_stable_for_idempotent_retry() {
        let identity = SynapseIdentity {
            fingerprint: "fp-a".to_string(),
            table_epoch: 7,
            equivalent_to: Vec::new(),
        };
        let items = vec![item("item:0", "hello")];
        assert_eq!(
            batch_request_key("configured-model", &identity, &items),
            batch_request_key("configured-model", &identity, &items)
        );
    }
}

/// Uses the embedding transport's daemon connection and management-route identity,
/// but does not require embedding capabilities or retry an interactive rerank call.
pub(crate) struct SynapseRerankTransport {
    runtime: Arc<SynapseRuntime>,
    state: SynapseState,
}

impl SynapseRerankTransport {
    pub(crate) fn new(config: &SemanticBackendConfig) -> Result<Self, SynapseEmbeddingError> {
        let connection_file = config
            .subc_connection_file
            .clone()
            .ok_or(SynapseEmbeddingError::MissingConnectionFile)?;
        validate_connection_file(&connection_file)?;
        let connection = SynapseConnection::new()?;
        let runtime = Arc::clone(&connection.runtime);
        Ok(Self {
            runtime,
            state: SynapseState {
                connection_file,
                route_project_root: config
                    .route_project_root
                    .clone()
                    .or_else(|| std::env::current_dir().ok())
                    .unwrap_or_else(|| PathBuf::from(".")),
                route_harness: config.route_harness.clone().unwrap_or_else(|| "aft".into()),
                model: config.model.clone(),
                call_timeout: Duration::from_millis(config.timeout_ms.max(1)),
                required_operations: &[MODELS_LIST_OPERATION, "rerank.score"],
                connection,
                consecutive_timeouts: 0,
                circuit_open_until: None,
            },
        })
    }

    pub(crate) fn call(
        &mut self,
        operation: &str,
        params: Value,
        deadline: Instant,
    ) -> Result<Value, SynapseEmbeddingError> {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .filter(|time| !time.is_zero())
            .ok_or_else(|| SynapseEmbeddingError::Timeout(operation.into()))?;
        self.runtime.block_on(async {
            // Include connect, catalog lookup, and route opening in the total budget.
            let result = tokio::time::timeout(remaining, async {
                let connection = self.state.ensure_route().await?;
                let body = serde_json::to_vec(&json!({"method":operation, "params":params}))
                    .map_err(|error| SynapseEmbeddingError::InvalidEnvelope(error.to_string()))?;
                let response = match connection
                    .consumer
                    .request(&connection.route, body, CallOptions::default())
                    .await
                {
                    Ok(response) => response,
                    Err(error) => {
                        if connection_is_gone(&error) {
                            self.state.connection.reset(&connection).await;
                        }
                        return Err(SynapseEmbeddingError::DaemonUnavailable(error.to_string()));
                    }
                };
                if response.len() > MAX_RESULT_PAGE_BYTES {
                    return Err(SynapseEmbeddingError::InvalidEnvelope(
                        "rerank.score response exceeded 512KiB".into(),
                    ));
                }
                serde_json::from_slice(&response)
                    .map_err(|error| SynapseEmbeddingError::InvalidEnvelope(error.to_string()))
            })
            .await;
            match result {
                Ok(result) => result,
                Err(_) => Err(SynapseEmbeddingError::Timeout(operation.into())),
            }
        })
    }
}
