use crate::cache_freshness::{self, FileFreshness, FreshnessVerdict};
use crate::config::{SemanticBackend, SemanticBackendConfig};
use crate::fs_lock;
use crate::parser::{detect_language, extract_symbols_from_tree, grammar_for};
use crate::search_index::{cache_relative_path, cached_path_under_root};
use crate::symbols::{Symbol, SymbolKind};
use crate::{slog_info, slog_warn};

use fastembed::{EmbeddingModel as FastembedEmbeddingModel, InitOptions, TextEmbedding};
use rayon::prelude::*;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::env;
use std::fmt::Display;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;
use std::time::SystemTime;
use tree_sitter::Parser;
use url::Url;

const DEFAULT_DIMENSION: usize = 384;
const MAX_ENTRIES: usize = 1_000_000;
// Covers high-dimensional backends such as OpenAI text-embedding-3-large (3072)
// and common local models (4096) while keeping a bounded supported shape.
const MAX_DIMENSION: usize = 4096;
const F32_BYTES: usize = std::mem::size_of::<f32>();
const HEADER_BYTES_V1: usize = 9;
const HEADER_BYTES_V2: usize = 13;
const ONNX_RUNTIME_INSTALL_HINT: &str =
    "ONNX Runtime not found. Install via: brew install onnxruntime (macOS) or apt install libonnxruntime (Linux).";

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
const DEFAULT_OPENAI_EMBEDDING_PATH: &str = "/embeddings";
const DEFAULT_OLLAMA_EMBEDDING_PATH: &str = "/api/embed";
// Must stay below the bridge timeout (30s) to avoid bridge kills on slow backends.
const DEFAULT_OPENAI_EMBEDDING_TIMEOUT_MS: u64 = 25_000;
const DEFAULT_MAX_BATCH_SIZE: usize = 64;
const QUERY_EMBEDDING_CACHE_CAP: usize = 1_000;
const FALLBACK_BACKEND: &str = "none";
const EMBEDDING_REQUEST_MAX_ATTEMPTS: usize = 3;
const EMBEDDING_REQUEST_BACKOFF_MS: [u64; 2] = [500, 1_000];
static SEMANTIC_LOCK_ACQUIRE_MUTEX: Mutex<()> = Mutex::new(());

pub struct SemanticIndexLock {
    _guard: fs_lock::LockGuard,
}

impl SemanticIndexLock {
    pub fn acquire(storage_dir: &Path, project_key: &str) -> std::io::Result<Self> {
        let dir = storage_dir.join("semantic").join(project_key);
        fs::create_dir_all(&dir)?;
        let path = dir.join("cache.lock");
        let _acquire_guard = SEMANTIC_LOCK_ACQUIRE_MUTEX
            .lock()
            .map_err(|_| std::io::Error::other("semantic cache lock acquisition mutex poisoned"))?;
        fs_lock::try_acquire(&path, Duration::from_secs(2))
            .map(|guard| Self { _guard: guard })
            .map_err(|error| match error {
                fs_lock::AcquireError::Timeout => {
                    std::io::Error::other("timed out acquiring semantic cache lock")
                }
                fs_lock::AcquireError::Io(error) => error,
            })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticIndexFingerprint {
    pub backend: String,
    pub model: String,
    #[serde(default)]
    pub base_url: String,
    pub dimension: usize,
    #[serde(default = "default_chunking_version")]
    pub chunking_version: u32,
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
        }
    }

    pub fn as_string(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| String::new())
    }

    fn matches_expected(&self, expected: &str) -> bool {
        let encoded = self.as_string();
        !encoded.is_empty() && encoded == expected
    }
}

enum SemanticEmbeddingEngine {
    Fastembed(TextEmbedding),
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
}

pub struct SemanticEmbeddingModel {
    backend: SemanticBackend,
    model: String,
    base_url: Option<String>,
    timeout_ms: u64,
    max_batch_size: usize,
    dimension: Option<usize>,
    engine: SemanticEmbeddingEngine,
    query_embedding_cache: HashMap<String, Vec<f32>>,
    query_embedding_cache_order: VecDeque<String>,
    query_embedding_cache_hits: u64,
    query_embedding_cache_misses: u64,
}

pub type EmbeddingModel = SemanticEmbeddingModel;

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

/// Returns true for IPv4/IPv6 addresses in private/link-local/CGNAT/wildcard
/// ranges, EXCLUDING loopback (127.0.0.0/8 and ::1). Loopback is considered
/// safe for SSRF purposes — see [`validate_base_url_no_ssrf`] for rationale.
fn is_private_non_loopback_ip(ip: &std::net::IpAddr) -> bool {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            // Note: 127.0.0.0/8 (loopback) is intentionally NOT in this set.
            // 10.0.0.0/8
            o[0] == 10
            // 172.16.0.0/12
            || (o[0] == 172 && (16..=31).contains(&o[1]))
            // 192.168.0.0/16
            || (o[0] == 192 && o[1] == 168)
            // 169.254.0.0/16 link-local
            || (o[0] == 169 && o[1] == 254)
            // 100.64.0.0/10 CGNAT
            || (o[0] == 100 && (64..=127).contains(&o[1]))
            // 0.0.0.0/8 wildcard
            || o[0] == 0
        }
        IpAddr::V6(v6) => {
            // Note: ::1 (loopback) is intentionally NOT in this set.
            let _ = Ipv6Addr::LOCALHOST; // touch to silence unused-import lints in some builds
                                         // fe80::/10 link-local
            (v6.segments()[0] & 0xffc0) == 0xfe80
            // fc00::/7 unique-local
            || (v6.segments()[0] & 0xfe00) == 0xfc00
            // ::ffff:0:0/96 IPv4-mapped — check the embedded IPv4
            || (v6.segments()[0] == 0 && v6.segments()[1] == 0
                && v6.segments()[2] == 0 && v6.segments()[3] == 0
                && v6.segments()[4] == 0 && v6.segments()[5] == 0xffff
                && {
                    let [a, b] = v6.segments()[6..8] else { return false; };
                    let ipv4 = Ipv4Addr::new((a >> 8) as u8, (a & 0xff) as u8, (b >> 8) as u8, (b & 0xff) as u8);
                    is_private_non_loopback_ip(&IpAddr::V4(ipv4))
                })
        }
    }
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

fn is_retryable_embedding_error(error: &reqwest::Error) -> bool {
    error.is_connect()
}

fn sleep_before_embedding_retry(attempt_index: usize) {
    if let Some(delay_ms) = EMBEDDING_REQUEST_BACKOFF_MS.get(attempt_index) {
        std::thread::sleep(Duration::from_millis(*delay_ms));
    }
}

fn send_embedding_request<F>(mut make_request: F, backend_label: &str) -> Result<String, String>
where
    F: FnMut() -> reqwest::blocking::RequestBuilder,
{
    for attempt_index in 0..EMBEDDING_REQUEST_MAX_ATTEMPTS {
        let last_attempt = attempt_index + 1 == EMBEDDING_REQUEST_MAX_ATTEMPTS;

        let response = match make_request().send() {
            Ok(response) => response,
            Err(error) => {
                if !last_attempt && is_retryable_embedding_error(&error) {
                    sleep_before_embedding_retry(attempt_index);
                    continue;
                }
                return Err(format!("{backend_label} request failed: {error}"));
            }
        };

        let status = response.status();
        let raw = match response.text() {
            Ok(raw) => raw,
            Err(error) => {
                if !last_attempt && is_retryable_embedding_error(&error) {
                    sleep_before_embedding_retry(attempt_index);
                    continue;
                }
                return Err(format!("{backend_label} response read failed: {error}"));
            }
        };

        if status.is_success() {
            return Ok(raw);
        }

        if !last_attempt && is_retryable_embedding_status(status) {
            sleep_before_embedding_retry(attempt_index);
            continue;
        }

        return Err(format!(
            "{backend_label} request failed (HTTP {}): {}",
            status, raw
        ));
    }

    unreachable!("embedding request retries exhausted without returning")
}

impl SemanticEmbeddingModel {
    pub fn from_config(config: &SemanticBackendConfig) -> Result<Self, String> {
        let timeout_ms = if config.timeout_ms == 0 {
            DEFAULT_OPENAI_EMBEDDING_TIMEOUT_MS
        } else {
            config.timeout_ms
        };

        let max_batch_size = if config.max_batch_size == 0 {
            DEFAULT_MAX_BATCH_SIZE
        } else {
            config.max_batch_size
        };

        let api_key_env = normalize_api_key(config.api_key_env.clone());
        let model = config.model.clone();

        let client = Client::builder()
            .timeout(Duration::from_millis(timeout_ms))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| format!("failed to configure embedding client: {error}"))?;

        let engine = match config.backend {
            SemanticBackend::Fastembed => {
                SemanticEmbeddingEngine::Fastembed(initialize_text_embedding(&model)?)
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
        };

        Ok(Self {
            backend: config.backend,
            model: config.model.clone(),
            base_url: config.base_url.clone(),
            timeout_ms,
            max_batch_size,
            dimension: None,
            engine,
            query_embedding_cache: HashMap::new(),
            query_embedding_cache_order: VecDeque::new(),
            query_embedding_cache_hits: 0,
            query_embedding_cache_misses: 0,
        })
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
        Ok(SemanticIndexFingerprint::from_config(config, dimension))
    }

    pub fn dimension(&mut self) -> Result<usize, String> {
        if let Some(dimension) = self.dimension {
            return Ok(dimension);
        }

        let dimension = match &mut self.engine {
            SemanticEmbeddingEngine::Fastembed(model) => {
                let vectors = model
                    .embed(vec!["semantic index fingerprint probe".to_string()], None)
                    .map_err(|error| format_embedding_init_error(error.to_string()))?;
                vectors
                    .first()
                    .map(|v| v.len())
                    .ok_or_else(|| "embedding backend returned no vectors".to_string())?
            }
            SemanticEmbeddingEngine::OpenAiCompatible { .. } => {
                let vectors =
                    self.embed_texts(vec!["semantic index fingerprint probe".to_string()])?;
                vectors
                    .first()
                    .map(|v| v.len())
                    .ok_or_else(|| "embedding backend returned no vectors".to_string())?
            }
            SemanticEmbeddingEngine::Ollama { .. } => {
                let vectors =
                    self.embed_texts(vec!["semantic index fingerprint probe".to_string()])?;
                vectors
                    .first()
                    .map(|v| v.len())
                    .ok_or_else(|| "embedding backend returned no vectors".to_string())?
            }
        };

        self.dimension = Some(dimension);
        Ok(dimension)
    }

    pub fn embed(&mut self, texts: Vec<String>) -> Result<Vec<Vec<f32>>, String> {
        self.embed_texts(texts)
    }

    pub fn embed_query_cached(&mut self, query: &str) -> Result<Vec<f32>, String> {
        if let Some(vector) = self.query_embedding_cache.get(query) {
            self.query_embedding_cache_hits += 1;
            return Ok(vector.clone());
        }

        self.query_embedding_cache_misses += 1;
        let embeddings = self.embed_texts(vec![query.to_string()])?;
        let vector = embeddings
            .first()
            .cloned()
            .ok_or_else(|| "embedding model returned no query vector".to_string())?;

        if self.query_embedding_cache.len() >= QUERY_EMBEDDING_CACHE_CAP {
            if let Some(oldest) = self.query_embedding_cache_order.pop_front() {
                self.query_embedding_cache.remove(&oldest);
            }
        }
        self.query_embedding_cache
            .insert(query.to_string(), vector.clone());
        self.query_embedding_cache_order
            .push_back(query.to_string());

        Ok(vector)
    }

    pub fn query_embedding_cache_stats(&self) -> (u64, u64, usize) {
        (
            self.query_embedding_cache_hits,
            self.query_embedding_cache_misses,
            self.query_embedding_cache.len(),
        )
    }

    fn embed_texts(&mut self, texts: Vec<String>) -> Result<Vec<Vec<f32>>, String> {
        match &mut self.engine {
            SemanticEmbeddingEngine::Fastembed(model) => model
                .embed(texts, None::<usize>)
                .map_err(|error| format_embedding_init_error(error.to_string()))
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
        }
    }
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
                    "ONNX Runtime not found. dlopen('{}') failed: {}. \
                     Run `npx @cortexkit/aft doctor` to diagnose.",
                    lib_name, msg
                ));
            }

            // Try to detect the runtime version from the file path or soname.
            // libonnxruntime.so.1.19.0, libonnxruntime.1.24.4.dylib, etc.
            let detected_version = detect_ort_version_from_path(lib_name);

            libc::dlclose(handle);

            // Check version compatibility — we need 1.24.x
            if let Some(ref version) = detected_version {
                let parts: Vec<&str> = version.split('.').collect();
                if let (Some(major), Some(minor)) = (
                    parts.first().and_then(|s| s.parse::<u32>().ok()),
                    parts.get(1).and_then(|s| s.parse::<u32>().ok()),
                ) {
                    if major != 1 || minor < 20 {
                        return Err(format_ort_version_mismatch(version, lib_name));
                    }
                }
            }
        }
    }

    #[cfg(target_os = "windows")]
    {
        // On Windows, skip pre-validation — let ort handle LoadLibrary
        let _ = dylib_path;
    }

    Ok(())
}

/// Try to extract the ORT version from the library filename or resolved symlink.
/// Examples: "libonnxruntime.so.1.19.0" → "1.19.0", "libonnxruntime.1.24.4.dylib" → "1.24.4"
#[cfg(any(test, target_os = "linux", target_os = "macos"))]
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
#[cfg(any(test, target_os = "linux", target_os = "macos"))]
fn extract_version_from_filename(name: &str) -> Option<String> {
    // Match patterns: .so.X.Y.Z or .X.Y.Z.dylib or .X.Y.Z.so
    let re = regex::Regex::new(r"(\d+\.\d+\.\d+)").ok()?;
    re.find(name).map(|m| m.as_str().to_string())
}

#[cfg(any(test, target_os = "linux", target_os = "macos"))]
fn suggest_removal_command(lib_path: &str) -> String {
    if lib_path.starts_with("/usr/local/lib")
        || lib_path == "libonnxruntime.so"
        || lib_path == "libonnxruntime.dylib"
    {
        #[cfg(target_os = "linux")]
        return "   sudo rm /usr/local/lib/libonnxruntime* && sudo ldconfig".to_string();
        #[cfg(target_os = "macos")]
        return "   sudo rm /usr/local/lib/libonnxruntime*".to_string();
        #[cfg(target_os = "windows")]
        return "   Delete the ONNX Runtime DLL from your PATH".to_string();
    }
    format!("   rm '{}'", lib_path)
}

/// Build the user-facing error message for an incompatible ONNX Runtime
/// install. Extracted as a pure helper so we can unit-test the wording
/// stability — the auto-fix recommendation must always come first because
/// it's the only safe option, and the system-rm step must remain present
/// because some users prefer the system-wide cleanup path.
#[cfg(any(test, target_os = "linux", target_os = "macos"))]
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

pub fn initialize_text_embedding(model: &str) -> Result<TextEmbedding, String> {
    // Pre-validate before ort can panic on a bad library
    pre_validate_onnx_runtime()?;

    let selected_model = match model {
        "all-MiniLM-L6-v2" | "all-minilm-l6-v2" => FastembedEmbeddingModel::AllMiniLML6V2,
        _ => {
            return Err(format!(
                "unsupported fastembed model '{}'. Supported: all-MiniLM-L6-v2",
                model
            ))
        }
    };

    TextEmbedding::try_new(InitOptions::new(selected_model)).map_err(format_embedding_init_error)
}

pub fn is_onnx_runtime_unavailable(message: &str) -> bool {
    if message.trim_start().starts_with("ONNX Runtime not found.") {
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

fn format_embedding_init_error(error: impl Display) -> String {
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
    /// Symbol kind (function, class, struct, etc.)
    pub kind: SymbolKind,
    /// Line range (0-based internally, inclusive)
    pub start_line: u32,
    pub end_line: u32,
    /// Whether the symbol is exported
    pub exported: bool,
    /// The enriched text that gets embedded (scope + signature + body snippet)
    pub embed_text: String,
    /// Short code snippet for display in results
    pub snippet: String,
}

/// A stored embedding entry — chunk metadata + vector
#[derive(Debug)]
struct EmbeddingEntry {
    chunk: SemanticChunk,
    vector: Vec<f32>,
}

/// The semantic index — stores embeddings for all symbols in a project
#[derive(Debug)]
pub struct SemanticIndex {
    entries: Vec<EmbeddingEntry>,
    /// Track which files are indexed and their mtime for staleness detection
    file_mtimes: HashMap<PathBuf, SystemTime>,
    /// Track indexed file sizes alongside mtimes for staleness detection
    file_sizes: HashMap<PathBuf, u64>,
    file_hashes: HashMap<PathBuf, blake3::Hash>,
    /// Embedding dimension (384 for MiniLM-L6-v2)
    dimension: usize,
    fingerprint: Option<SemanticIndexFingerprint>,
    project_root: PathBuf,
}

#[derive(Debug, Clone, Copy)]
struct IndexedFileMetadata {
    mtime: SystemTime,
    size: u64,
    content_hash: blake3::Hash,
}

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

/// Search result from a semantic query
#[derive(Debug, Clone)]
pub struct SemanticResult {
    pub file: PathBuf,
    pub name: String,
    pub kind: SymbolKind,
    pub start_line: u32,
    pub end_line: u32,
    pub exported: bool,
    pub snippet: String,
    pub score: f32,
    pub source: &'static str,
}

impl SemanticIndex {
    pub fn new(project_root: PathBuf, dimension: usize) -> Self {
        debug_assert!(project_root.is_absolute());
        Self {
            entries: Vec::new(),
            file_mtimes: HashMap::new(),
            file_sizes: HashMap::new(),
            file_hashes: HashMap::new(),
            dimension,
            fingerprint: None,
            project_root,
        }
    }

    /// Number of embedded symbol entries.
    pub fn entry_count(&self) -> usize {
        self.entries.len()
    }

    /// Human-readable status label for the index.
    pub fn status_label(&self) -> &'static str {
        if self.entries.is_empty() {
            "empty"
        } else {
            "ready"
        }
    }

    fn collect_chunks(
        project_root: &Path,
        files: &[PathBuf],
    ) -> (Vec<SemanticChunk>, HashMap<PathBuf, IndexedFileMetadata>) {
        let per_file: Vec<(
            PathBuf,
            Result<(IndexedFileMetadata, Vec<SemanticChunk>), String>,
        )> = files
            .par_iter()
            .map_init(HashMap::new, |parsers, file| {
                let result = collect_file_metadata(file).and_then(|metadata| {
                    collect_file_chunks(project_root, file, parsers)
                        .map(|chunks| (metadata, chunks))
                });
                (file.clone(), result)
            })
            .collect();

        let mut chunks: Vec<SemanticChunk> = Vec::new();
        let mut file_metadata: HashMap<PathBuf, IndexedFileMetadata> = HashMap::new();

        for (file, result) in per_file {
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

        (chunks, file_metadata)
    }

    fn build_from_chunks<F, P>(
        project_root: &Path,
        chunks: Vec<SemanticChunk>,
        file_metadata: HashMap<PathBuf, IndexedFileMetadata>,
        embed_fn: &mut F,
        max_batch_size: usize,
        mut progress: Option<&mut P>,
    ) -> Result<Self, String>
    where
        F: FnMut(Vec<String>) -> Result<Vec<Vec<f32>>, String>,
        P: FnMut(usize, usize),
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
                file_hashes: file_metadata
                    .into_iter()
                    .map(|(path, metadata)| (path, metadata.content_hash))
                    .collect(),
                dimension: DEFAULT_DIMENSION,
                fingerprint: None,
                project_root: project_root.to_path_buf(),
            });
        }

        // Embed in batches
        let mut entries: Vec<EmbeddingEntry> = Vec::with_capacity(chunks.len());
        let mut expected_dimension: Option<usize> = None;
        let batch_size = max_batch_size.max(1);
        for batch_start in (0..chunks.len()).step_by(batch_size) {
            let batch_end = (batch_start + batch_size).min(chunks.len());
            let batch_texts: Vec<String> = chunks[batch_start..batch_end]
                .iter()
                .map(|c| c.embed_text.clone())
                .collect();

            let vectors = embed_fn(batch_texts)?;
            validate_embedding_batch(&vectors, batch_end - batch_start, "embedding backend")?;

            // Track consistent dimension across all batches
            if let Some(dim) = vectors.first().map(|v| v.len()) {
                match expected_dimension {
                    None => expected_dimension = Some(dim),
                    Some(expected) if dim != expected => {
                        return Err(format!(
                            "embedding dimension changed across batches: expected {expected}, got {dim}"
                        ));
                    }
                    _ => {}
                }
            }

            for (i, vector) in vectors.into_iter().enumerate() {
                let chunk_idx = batch_start + i;
                entries.push(EmbeddingEntry {
                    chunk: chunks[chunk_idx].clone(),
                    vector,
                });
            }

            if let Some(callback) = progress.as_mut() {
                callback(entries.len(), total_chunks);
            }
        }

        let dimension = entries
            .first()
            .map(|e| e.vector.len())
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
            file_hashes: file_metadata
                .into_iter()
                .map(|(path, metadata)| (path, metadata.content_hash))
                .collect(),
            dimension,
            fingerprint: None,
            project_root: project_root.to_path_buf(),
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
        let (chunks, file_mtimes) = Self::collect_chunks(project_root, files);
        Self::build_from_chunks(
            project_root,
            chunks,
            file_mtimes,
            embed_fn,
            max_batch_size,
            Option::<&mut fn(usize, usize)>::None,
        )
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
        let (chunks, file_mtimes) = Self::collect_chunks(project_root, files);
        let total_chunks = chunks.len();
        progress(0, total_chunks);
        Self::build_from_chunks(
            project_root,
            chunks,
            file_mtimes,
            embed_fn,
            max_batch_size,
            Some(progress),
        )
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
        self.backfill_missing_file_sizes();

        // 1. Bucket files into deleted / changed / added.
        let current_set: HashSet<&Path> = current_files.iter().map(PathBuf::as_path).collect();
        let total_processed = current_set.len() + self.file_mtimes.len()
            - self
                .file_mtimes
                .keys()
                .filter(|path| current_set.contains(path.as_path()))
                .count();

        // Files in cache that disappeared from disk OR are no longer in the
        // walked set. Both cases need their entries dropped.
        let mut deleted: Vec<PathBuf> = Vec::new();
        let mut changed: Vec<PathBuf> = Vec::new();
        let indexed_paths: Vec<PathBuf> = self.file_mtimes.keys().cloned().collect();
        for indexed_path in &indexed_paths {
            if !current_set.contains(indexed_path.as_path()) {
                deleted.push(indexed_path.clone());
                continue;
            }
            let cached = match (
                self.file_mtimes.get(indexed_path),
                self.file_sizes.get(indexed_path),
                self.file_hashes.get(indexed_path),
            ) {
                (Some(mtime), Some(size), Some(hash)) => Some(FileFreshness {
                    mtime: *mtime,
                    size: *size,
                    content_hash: *hash,
                }),
                _ => None,
            };
            match cached.map(|freshness| cache_freshness::verify_file(indexed_path, &freshness)) {
                Some(FreshnessVerdict::HotFresh) => {}
                Some(FreshnessVerdict::ContentFresh {
                    new_mtime,
                    new_size,
                }) => {
                    self.file_mtimes.insert(indexed_path.clone(), new_mtime);
                    self.file_sizes.insert(indexed_path.clone(), new_size);
                }
                Some(FreshnessVerdict::Stale | FreshnessVerdict::Deleted) | None => {
                    changed.push(indexed_path.clone());
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
            let deleted_set: HashSet<&Path> = deleted.iter().map(PathBuf::as_path).collect();
            self.entries
                .retain(|entry| !deleted_set.contains(entry.chunk.file.as_path()));
            for path in &deleted {
                self.file_mtimes.remove(path);
                self.file_sizes.remove(path);
                self.file_hashes.remove(path);
            }
        }

        // 3. Embed the changed + added set, if any.
        let mut to_embed: Vec<PathBuf> = Vec::with_capacity(changed.len() + added.len());
        to_embed.extend(changed.iter().cloned());
        to_embed.extend(added.iter().cloned());

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

        let (chunks, fresh_metadata) = Self::collect_chunks(project_root, &to_embed);

        if chunks.is_empty() {
            progress(0, 0);
            let successful_files: HashSet<PathBuf> = fresh_metadata.keys().cloned().collect();
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
            return Ok(RefreshSummary {
                changed: changed_count,
                added: added_count,
                deleted: deleted.len(),
                total_processed,
            });
        }

        // 4. Embed in batches and dimension-check against the existing index.
        let total_chunks = chunks.len();
        progress(0, total_chunks);
        let batch_size = max_batch_size.max(1);
        let existing_dimension = if self.entries.is_empty() {
            None
        } else {
            Some(self.dimension)
        };
        let mut new_entries: Vec<EmbeddingEntry> = Vec::with_capacity(chunks.len());
        let mut observed_dimension: Option<usize> = existing_dimension;

        for batch_start in (0..chunks.len()).step_by(batch_size) {
            let batch_end = (batch_start + batch_size).min(chunks.len());
            let batch_texts: Vec<String> = chunks[batch_start..batch_end]
                .iter()
                .map(|c| c.embed_text.clone())
                .collect();

            let vectors = embed_fn(batch_texts)?;
            validate_embedding_batch(&vectors, batch_end - batch_start, "embedding backend")?;

            if let Some(dim) = vectors.first().map(|v| v.len()) {
                match observed_dimension {
                    None => observed_dimension = Some(dim),
                    Some(expected) if dim != expected => {
                        // Refuse to mix dimensions in one index. Caller should
                        // fall back to a full rebuild.
                        return Err(format!(
                            "embedding dimension changed during incremental refresh: \
                             cached index uses {expected}, new vectors use {dim}"
                        ));
                    }
                    _ => {}
                }
            }

            for (i, vector) in vectors.into_iter().enumerate() {
                let chunk_idx = batch_start + i;
                new_entries.push(EmbeddingEntry {
                    chunk: chunks[chunk_idx].clone(),
                    vector,
                });
            }

            progress(new_entries.len(), total_chunks);
        }

        let successful_files: HashSet<PathBuf> = fresh_metadata.keys().cloned().collect();
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

    /// Search the index with a query embedding, returning top-K results sorted by relevance
    pub fn search(&self, query_vector: &[f32], top_k: usize) -> Vec<SemanticResult> {
        if self.entries.is_empty() || query_vector.len() != self.dimension {
            return Vec::new();
        }

        let mut scored: Vec<(f32, usize)> = self
            .entries
            .iter()
            .enumerate()
            .map(|(i, entry)| {
                let mut score = cosine_similarity(query_vector, &entry.vector);
                if entry.chunk.exported {
                    score *= 1.1;
                }
                (score, i)
            })
            .collect();

        // Sort descending by score
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

        scored
            .into_iter()
            .take(top_k)
            // Keep the sort → take → map ordering explicit: removing the old
            // `> 0.0` floor cannot evict positive hits because top_k has already
            // been selected, but it can surface zero-score noise in the tail.
            .map(|(score, idx)| {
                let entry = &self.entries[idx];
                SemanticResult {
                    file: entry.chunk.file.clone(),
                    name: entry.chunk.name.clone(),
                    kind: entry.chunk.kind.clone(),
                    start_line: entry.chunk.start_line,
                    end_line: entry.chunk.end_line,
                    exported: entry.chunk.exported,
                    snippet: entry.chunk.snippet.clone(),
                    score,
                    source: "semantic",
                }
            })
            .collect()
    }

    /// Number of indexed entries
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Check if a file needs re-indexing based on mtime/size
    pub fn is_file_stale(&self, file: &Path) -> bool {
        let Some(stored_mtime) = self.file_mtimes.get(file) else {
            return true;
        };
        let Some(stored_size) = self.file_sizes.get(file) else {
            return true;
        };
        let Some(stored_hash) = self.file_hashes.get(file) else {
            return true;
        };
        let cached = FileFreshness {
            mtime: *stored_mtime,
            size: *stored_size,
            content_hash: *stored_hash,
        };
        match cache_freshness::verify_file(file, &cached) {
            FreshnessVerdict::HotFresh => false,
            FreshnessVerdict::ContentFresh { .. } => false,
            FreshnessVerdict::Stale | FreshnessVerdict::Deleted => true,
        }
    }

    fn backfill_missing_file_sizes(&mut self) {
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
    }

    /// Remove entries for a specific file
    pub fn remove_file(&mut self, file: &Path) {
        self.invalidate_file(file);
    }

    pub fn invalidate_file(&mut self, file: &Path) {
        self.entries.retain(|e| e.chunk.file != file);
        self.file_mtimes.remove(file);
        self.file_sizes.remove(file);
        self.file_hashes.remove(file);
    }

    /// Get the embedding dimension
    pub fn dimension(&self) -> usize {
        self.dimension
    }

    pub fn fingerprint(&self) -> Option<&SemanticIndexFingerprint> {
        self.fingerprint.as_ref()
    }

    pub fn backend_label(&self) -> Option<&str> {
        self.fingerprint.as_ref().map(|f| f.backend.as_str())
    }

    pub fn model_label(&self) -> Option<&str> {
        self.fingerprint.as_ref().map(|f| f.model.as_str())
    }

    pub fn set_fingerprint(&mut self, fingerprint: SemanticIndexFingerprint) {
        self.fingerprint = Some(fingerprint);
    }

    /// Write the semantic index to disk using atomic temp+rename pattern
    pub fn write_to_disk(&self, storage_dir: &Path, project_key: &str) {
        // Don't persist empty indexes — they would be loaded on next startup
        // and prevent a fresh build that might find files.
        if self.entries.is_empty() {
            slog_info!("skipping semantic index persistence (0 entries)");
            return;
        }
        let dir = storage_dir.join("semantic").join(project_key);
        if let Err(e) = fs::create_dir_all(&dir) {
            slog_warn!("failed to create semantic cache dir: {}", e);
            return;
        }
        let data_path = dir.join("semantic.bin");
        let tmp_path = dir.join(format!(
            "semantic.bin.tmp.{}.{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or(Duration::ZERO)
                .as_nanos()
        ));
        let bytes = self.to_bytes();
        let write_result = (|| -> std::io::Result<()> {
            use std::io::Write;
            let mut file = fs::File::create(&tmp_path)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            Ok(())
        })();
        if let Err(e) = write_result {
            slog_warn!("failed to write semantic index: {}", e);
            let _ = fs::remove_file(&tmp_path);
            return;
        }
        if let Err(e) = fs::rename(&tmp_path, &data_path) {
            slog_warn!("failed to rename semantic index: {}", e);
            let _ = fs::remove_file(&tmp_path);
            return;
        }
        slog_info!(
            "semantic index persisted: {} entries, {:.1} KB",
            self.entries.len(),
            bytes.len() as f64 / 1024.0
        );
    }

    /// Read the semantic index from disk
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
        let file_len = usize::try_from(fs::metadata(&data_path).ok()?.len()).ok()?;
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

        let bytes = fs::read(&data_path).ok()?;
        let version = bytes[0];
        if version != SEMANTIC_INDEX_VERSION_V6 {
            slog_info!(
                "cached semantic index version {} is older than {}, rebuilding",
                version,
                SEMANTIC_INDEX_VERSION_V6
            );
            if !is_worktree_bridge {
                let _ = fs::remove_file(&data_path);
            }
            return None;
        }
        match Self::from_bytes(&bytes, current_canonical_root) {
            Ok(index) => {
                if index.entries.is_empty() {
                    slog_info!("cached semantic index is empty, will rebuild");
                    if !is_worktree_bridge {
                        let _ = fs::remove_file(&data_path);
                    }
                    return None;
                }
                if let Some(expected) = expected_fingerprint {
                    let matches = index
                        .fingerprint()
                        .map(|fingerprint| fingerprint.matches_expected(expected))
                        .unwrap_or(false);
                    if !matches {
                        slog_info!("cached semantic index fingerprint mismatch, rebuilding");
                        if !is_worktree_bridge {
                            let _ = fs::remove_file(&data_path);
                        }
                        return None;
                    }
                }
                slog_info!(
                    "loaded semantic index from disk: {} entries",
                    index.entries.len()
                );
                Some(index)
            }
            Err(e) => {
                slog_warn!("corrupt semantic index, rebuilding: {}", e);
                if !is_worktree_bridge {
                    let _ = fs::remove_file(&data_path);
                }
                None
            }
        }
    }

    /// Serialize the index to bytes for disk persistence
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        let fingerprint_bytes = self.fingerprint.as_ref().and_then(|fingerprint| {
            let encoded = fingerprint.as_string();
            if encoded.is_empty() {
                None
            } else {
                Some(encoded.into_bytes())
            }
        });
        let file_mtimes: Vec<_> = self
            .file_mtimes
            .iter()
            .filter_map(|(path, mtime)| {
                cache_relative_path(&self.project_root, path)
                    .map(|relative| (relative, path, mtime))
            })
            .collect();
        let entries: Vec<_> = self
            .entries
            .iter()
            .filter_map(|entry| {
                cache_relative_path(&self.project_root, &entry.chunk.file)
                    .map(|relative| (relative, entry))
            })
            .collect();

        // Header: version(1) + dimension(4) + entry_count(4) + fingerprint_len(4) + fingerprint
        //
        // V6 is the single write format. Layout extends V5:
        //   - fingerprint is always represented (absent ⇒ fingerprint_len=0,
        //     no bytes follow). Uniform format simplifies the reader.
        //   - paths are relative to project_root.
        //   - file metadata stored as secs(u64) + subsec_nanos(u32) + size(u64) + blake3(32).
        //     Preserves full APFS/ext4/NTFS precision and catches mtime ties.
        //
        // V1/V2 remain readable for backward compatibility (see from_bytes).
        // V3/V4 load as compatible formats but are rejected on disk so snippets
        // and file sizes are rebuilt once.
        let version = SEMANTIC_INDEX_VERSION_V6;
        buf.push(version);
        buf.extend_from_slice(&(self.dimension as u32).to_le_bytes());
        buf.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        let fp_bytes_ref: &[u8] = fingerprint_bytes.as_deref().unwrap_or(&[]);
        buf.extend_from_slice(&(fp_bytes_ref.len() as u32).to_le_bytes());
        buf.extend_from_slice(fp_bytes_ref);

        // File mtime table: count(4) + entries
        // V3 layout per entry: path_len(4) + path + secs(8) + subsec_nanos(4)
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
            let size = self.file_sizes.get(*path).copied().unwrap_or_default();
            buf.extend_from_slice(&size.to_le_bytes());
            let hash = self
                .file_hashes
                .get(*path)
                .copied()
                .unwrap_or_else(cache_freshness::zero_hash);
            buf.extend_from_slice(hash.as_bytes());
        }

        // Entries: each is metadata + vector
        for (relative, entry) in &entries {
            let c = &entry.chunk;

            // File path
            let file_bytes = relative.to_string_lossy().as_bytes().to_vec();
            buf.extend_from_slice(&(file_bytes.len() as u32).to_le_bytes());
            buf.extend_from_slice(&file_bytes);

            // Name
            let name_bytes = c.name.as_bytes();
            buf.extend_from_slice(&(name_bytes.len() as u32).to_le_bytes());
            buf.extend_from_slice(name_bytes);

            // Kind (1 byte)
            buf.push(symbol_kind_to_u8(&c.kind));

            // Lines + exported
            buf.extend_from_slice(&(c.start_line as u32).to_le_bytes());
            buf.extend_from_slice(&(c.end_line as u32).to_le_bytes());
            buf.push(c.exported as u8);

            // Snippet
            let snippet_bytes = c.snippet.as_bytes();
            buf.extend_from_slice(&(snippet_bytes.len() as u32).to_le_bytes());
            buf.extend_from_slice(snippet_bytes);

            // Embed text
            let embed_bytes = c.embed_text.as_bytes();
            buf.extend_from_slice(&(embed_bytes.len() as u32).to_le_bytes());
            buf.extend_from_slice(embed_bytes);

            // Vector (f32 array)
            for &val in &entry.vector {
                buf.extend_from_slice(&val.to_le_bytes());
            }
        }

        buf
    }

    /// Deserialize the index from bytes
    pub fn from_bytes(data: &[u8], current_canonical_root: &Path) -> Result<Self, String> {
        debug_assert!(current_canonical_root.is_absolute());
        let mut pos = 0;

        if data.len() < HEADER_BYTES_V1 {
            return Err("data too short".to_string());
        }

        let version = data[pos];
        pos += 1;
        if version != SEMANTIC_INDEX_VERSION_V1
            && version != SEMANTIC_INDEX_VERSION_V2
            && version != SEMANTIC_INDEX_VERSION_V3
            && version != SEMANTIC_INDEX_VERSION_V4
            && version != SEMANTIC_INDEX_VERSION_V5
            && version != SEMANTIC_INDEX_VERSION_V6
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
            || version == SEMANTIC_INDEX_VERSION_V6)
            && data.len() < HEADER_BYTES_V2
        {
            return Err("data too short for semantic index v2/v3/v4/v5/v6 header".to_string());
        }

        let dimension = read_u32(data, &mut pos)? as usize;
        let entry_count = read_u32(data, &mut pos)? as usize;
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
            || version == SEMANTIC_INDEX_VERSION_V6;
        let fingerprint = if has_fingerprint_field {
            let fingerprint_len = read_u32(data, &mut pos)? as usize;
            if pos + fingerprint_len > data.len() {
                return Err("unexpected end of data reading fingerprint".to_string());
            }
            if fingerprint_len == 0 {
                None
            } else {
                let raw = String::from_utf8_lossy(&data[pos..pos + fingerprint_len]).to_string();
                pos += fingerprint_len;
                Some(
                    serde_json::from_str::<SemanticIndexFingerprint>(&raw)
                        .map_err(|error| format!("invalid semantic fingerprint: {error}"))?,
                )
            }
        } else {
            None
        };

        // File mtimes
        let mtime_count = read_u32(data, &mut pos)? as usize;
        if mtime_count > MAX_ENTRIES {
            return Err(format!("too many semantic file mtimes: {}", mtime_count));
        }

        let vector_bytes = entry_count
            .checked_mul(dimension)
            .and_then(|count| count.checked_mul(F32_BYTES))
            .ok_or_else(|| "semantic vector allocation overflow".to_string())?;
        if vector_bytes > data.len().saturating_sub(pos) {
            return Err("semantic index vectors exceed available data".to_string());
        }

        let mut file_mtimes = HashMap::with_capacity(mtime_count);
        let mut file_sizes = HashMap::with_capacity(mtime_count);
        let mut file_hashes = HashMap::with_capacity(mtime_count);
        for _ in 0..mtime_count {
            let path = read_string(data, &mut pos)?;
            let secs = read_u64(data, &mut pos)?;
            // V3+ persists subsec_nanos alongside secs so staleness checks
            // survive restart round-trips. V1/V2 load with 0 nanos, which
            // causes one rebuild on upgrade (they never matched live APFS
            // mtimes anyway — the bug v0.15.2 fixes). After that rebuild,
            // the cache is persisted as V3 and stabilises.
            let nanos = if version == SEMANTIC_INDEX_VERSION_V3
                || version == SEMANTIC_INDEX_VERSION_V4
                || version == SEMANTIC_INDEX_VERSION_V5
                || version == SEMANTIC_INDEX_VERSION_V6
            {
                read_u32(data, &mut pos)?
            } else {
                0
            };
            let size =
                if version == SEMANTIC_INDEX_VERSION_V5 || version == SEMANTIC_INDEX_VERSION_V6 {
                    read_u64(data, &mut pos)?
                } else {
                    0
                };
            let content_hash = if version == SEMANTIC_INDEX_VERSION_V6 {
                if pos + 32 > data.len() {
                    return Err("unexpected end of data reading content hash".to_string());
                }
                let mut hash_bytes = [0u8; 32];
                hash_bytes.copy_from_slice(&data[pos..pos + 32]);
                pos += 32;
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
            let path = if version == SEMANTIC_INDEX_VERSION_V6 {
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
            let raw_file = PathBuf::from(read_string(data, &mut pos)?);
            let file = if version == SEMANTIC_INDEX_VERSION_V6 {
                cached_path_under_root(current_canonical_root, &raw_file)
                    .ok_or_else(|| "cached semantic entry path escapes project root".to_string())?
            } else {
                raw_file
            };
            let name = read_string(data, &mut pos)?;

            if pos >= data.len() {
                return Err("unexpected end of data".to_string());
            }
            let kind = u8_to_symbol_kind(data[pos]);
            pos += 1;

            let start_line = read_u32(data, &mut pos)?;
            let end_line = read_u32(data, &mut pos)?;

            if pos >= data.len() {
                return Err("unexpected end of data".to_string());
            }
            let exported = data[pos] != 0;
            pos += 1;

            let snippet = read_string(data, &mut pos)?;
            let embed_text = read_string(data, &mut pos)?;

            // Vector
            let vec_bytes = dimension
                .checked_mul(F32_BYTES)
                .ok_or_else(|| "semantic vector allocation overflow".to_string())?;
            if pos + vec_bytes > data.len() {
                return Err("unexpected end of data reading vector".to_string());
            }
            let mut vector = Vec::with_capacity(dimension);
            for _ in 0..dimension {
                let bytes = [data[pos], data[pos + 1], data[pos + 2], data[pos + 3]];
                vector.push(f32::from_le_bytes(bytes));
                pos += 4;
            }

            entries.push(EmbeddingEntry {
                chunk: SemanticChunk {
                    file,
                    name,
                    kind,
                    start_line,
                    end_line,
                    exported,
                    embed_text,
                    snippet,
                },
                vector,
            });
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

        Ok(Self {
            entries,
            file_mtimes,
            file_sizes,
            file_hashes,
            dimension,
            fingerprint,
            project_root: current_canonical_root.to_path_buf(),
        })
    }
}

/// Build enriched embedding text from a symbol with cAST-style context
fn build_embed_text(symbol: &Symbol, source: &str, file: &Path, project_root: &Path) -> String {
    let relative = file
        .strip_prefix(project_root)
        .unwrap_or(file)
        .to_string_lossy();

    let kind_label = match &symbol.kind {
        SymbolKind::Function => "function",
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
        text.push_str(&format!(" signature:{}", sig));
    }

    // Add body snippet (first ~300 chars of symbol body)
    let lines: Vec<&str> = source.lines().collect();
    let start = (symbol.range.start_line as usize).min(lines.len());
    // range.end_line is inclusive 0-based; +1 makes it an exclusive slice bound.
    let end = (symbol.range.end_line as usize + 1).min(lines.len());
    if start < end {
        let body: String = lines[start..end]
            .iter()
            .take(15) // max 15 lines
            .copied()
            .collect::<Vec<&str>>()
            .join("\n");
        let snippet = if body.len() > 300 {
            format!("{}...", &body[..body.floor_char_boundary(300)])
        } else {
            body
        };
        text.push_str(&format!(" body:{}", snippet));
    }

    text
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

fn first_leading_doc_comment(source: &str) -> String {
    let lines: Vec<&str> = source.lines().collect();
    let Some((start, first)) = lines
        .iter()
        .enumerate()
        .find(|(_, line)| !line.trim().is_empty())
    else {
        return String::new();
    };

    let trimmed = first.trim_start();
    if trimmed.starts_with("/**") {
        let mut comment = Vec::new();
        for line in lines.iter().skip(start) {
            comment.push(*line);
            if line.contains("*/") {
                break;
            }
        }
        return truncate_chars(&comment.join("\n"), 200);
    }

    if trimmed.starts_with("///") || trimmed.starts_with("//!") {
        let comment = lines
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
    let doc = first_leading_doc_comment(source);
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
        kind: SymbolKind::FileSummary,
        start_line: 0,
        end_line: 0,
        exported: false,
        embed_text: format!(
            "file:{rel_path} kind:file-summary name:{} parent:{parent_dir} doc:{doc} exports:{exports}",
            file.file_stem()
                .map(|stem| stem.to_string_lossy().to_string())
                .unwrap_or_default()
        ),
        snippet,
    }
}

fn parser_for(
    parsers: &mut HashMap<crate::parser::LangId, Parser>,
    lang: crate::parser::LangId,
) -> Result<&mut Parser, String> {
    use std::collections::hash_map::Entry;

    match parsers.entry(lang) {
        Entry::Occupied(entry) => Ok(entry.into_mut()),
        Entry::Vacant(entry) => {
            let grammar = grammar_for(lang);
            let mut parser = Parser::new();
            parser
                .set_language(&grammar)
                .map_err(|error| error.to_string())?;
            Ok(entry.insert(parser))
        }
    }
}

pub fn is_semantic_indexed_extension(path: &Path) -> bool {
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
                | "zig"
                | "cs"
                | "sh"
                | "bash"
                | "zsh"
                | "sol"
                | "vue"
        )
    )
}

fn collect_file_metadata(file: &Path) -> Result<IndexedFileMetadata, String> {
    let metadata = fs::metadata(file).map_err(|error| error.to_string())?;
    let mtime = metadata.modified().map_err(|error| error.to_string())?;
    let content_hash = cache_freshness::hash_file_if_small(file, metadata.len())
        .map_err(|error| error.to_string())?
        .unwrap_or_else(cache_freshness::zero_hash);
    Ok(IndexedFileMetadata {
        mtime,
        size: metadata.len(),
        content_hash,
    })
}

fn collect_file_chunks(
    project_root: &Path,
    file: &Path,
    parsers: &mut HashMap<crate::parser::LangId, Parser>,
) -> Result<Vec<SemanticChunk>, String> {
    if !is_semantic_indexed_extension(file) {
        return Err("unsupported file extension".to_string());
    }
    let lang = detect_language(file).ok_or_else(|| "unsupported file extension".to_string())?;
    let source = std::fs::read_to_string(file).map_err(|error| error.to_string())?;
    let tree = parser_for(parsers, lang)?
        .parse(&source, None)
        .ok_or_else(|| format!("tree-sitter parse returned None for {}", file.display()))?;
    let symbols =
        extract_symbols_from_tree(&source, &tree, lang).map_err(|error| error.to_string())?;

    Ok(symbols_to_chunks(file, &symbols, &source, project_root))
}

/// Build a display snippet from a symbol's source
fn build_snippet(symbol: &Symbol, source: &str) -> String {
    let lines: Vec<&str> = source.lines().collect();
    let start = (symbol.range.start_line as usize).min(lines.len());
    // range.end_line is inclusive 0-based; +1 makes it an exclusive slice bound.
    let end = (symbol.range.end_line as usize + 1).min(lines.len());
    if start < end {
        let snippet_lines: Vec<&str> = lines[start..end].iter().take(5).copied().collect();
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

/// Convert symbols to semantic chunks with enriched context
fn symbols_to_chunks(
    file: &Path,
    symbols: &[Symbol],
    source: &str,
    project_root: &Path,
) -> Vec<SemanticChunk> {
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
        chunks.push(build_file_summary_chunk(
            file,
            project_root,
            source,
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

        let embed_text = build_embed_text(symbol, source, file, project_root);
        let snippet = build_snippet(symbol, source);

        chunks.push(SemanticChunk {
            file: file.to_path_buf(),
            name: symbol.name.clone(),
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

/// Cosine similarity between two vectors
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
        _ => SymbolKind::Heading,
    }
}

fn read_u32(data: &[u8], pos: &mut usize) -> Result<u32, String> {
    if *pos + 4 > data.len() {
        return Err("unexpected end of data reading u32".to_string());
    }
    let val = u32::from_le_bytes([data[*pos], data[*pos + 1], data[*pos + 2], data[*pos + 3]]);
    *pos += 4;
    Ok(val)
}

fn read_u64(data: &[u8], pos: &mut usize) -> Result<u64, String> {
    if *pos + 8 > data.len() {
        return Err("unexpected end of data reading u64".to_string());
    }
    let bytes: [u8; 8] = data[*pos..*pos + 8].try_into().unwrap();
    *pos += 8;
    Ok(u64::from_le_bytes(bytes))
}

fn read_string(data: &[u8], pos: &mut usize) -> Result<String, String> {
    let len = read_u32(data, pos)? as usize;
    if *pos + len > data.len() {
        return Err("unexpected end of data reading string".to_string());
    }
    let s = String::from_utf8_lossy(&data[*pos..*pos + len]).to_string();
    *pos += len;
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{SemanticBackend, SemanticBackendConfig};
    use crate::parser::FileParser;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

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

    fn set_file_metadata(index: &mut SemanticIndex, file: &Path, mtime: SystemTime, size: u64) {
        index.file_mtimes.insert(file.to_path_buf(), mtime);
        index.file_sizes.insert(file.to_path_buf(), size);
        index
            .file_hashes
            .insert(file.to_path_buf(), cache_freshness::zero_hash());
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
                kind: SymbolKind::Function,
                start_line: 0,
                end_line: 0,
                exported: false,
                embed_text: "outside".to_string(),
                snippet: "outside".to_string(),
            },
            vector: vec![1.0, 0.0, 0.0],
        });

        let bytes = index.to_bytes();
        let loaded = SemanticIndex::from_bytes(&bytes, &project).expect("load serialized index");
        assert_eq!(loaded.entries.len(), 0);
        assert!(loaded.file_mtimes.is_empty());
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
                kind: SymbolKind::Function,
                start_line: 10,
                end_line: 25,
                exported: true,
                embed_text: "file:src/main.rs kind:function name:handle_request".to_string(),
                snippet: "fn handle_request() {\n  // ...\n}".to_string(),
            },
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
        });

        let bytes = index.to_bytes();
        let restored = SemanticIndex::from_bytes(&bytes, &project_root).unwrap();

        assert_eq!(restored.entries.len(), 1);
        assert_eq!(restored.entries[0].chunk.name, "handle_request");
        assert_eq!(restored.entries[0].vector, vec![0.1, 0.2, 0.3, 0.4]);
        assert_eq!(restored.dimension, 4);
        assert_eq!(restored.backend_label(), Some("fastembed"));
        assert_eq!(restored.model_label(), Some("all-MiniLM-L6-v2"));
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
                    kind: SymbolKind::Function,
                    start_line: (i * 10 + 1) as u32,
                    end_line: (i * 10 + 5) as u32,
                    exported: true,
                    embed_text: format!("kind:function name:{}", name),
                    snippet: format!("fn {}() {{}}", name),
                },
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
    fn optimized_file_chunk_collection_matches_file_parser_path() {
        let project_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let file = project_root.join("src/semantic_index.rs");
        let source = std::fs::read_to_string(&file).unwrap();

        let mut legacy_parser = FileParser::new();
        let legacy_symbols = legacy_parser.extract_symbols(&file).unwrap();
        let legacy_chunks = symbols_to_chunks(&file, &legacy_symbols, &source, &project_root);

        let mut parsers = HashMap::new();
        let optimized_chunks = collect_file_chunks(&project_root, &file, &mut parsers).unwrap();

        assert_eq!(
            chunk_fingerprint(&optimized_chunks),
            chunk_fingerprint(&legacy_chunks)
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

    #[test]
    fn invalidate_file_removes_entries_and_mtime() {
        let target = PathBuf::from("/src/main.rs");
        let mut index = SemanticIndex::new(test_project_root(), DEFAULT_DIMENSION);
        index.entries.push(EmbeddingEntry {
            chunk: SemanticChunk {
                file: target.clone(),
                name: "main".to_string(),
                kind: SymbolKind::Function,
                start_line: 0,
                end_line: 1,
                exported: false,
                embed_text: "main".to_string(),
                snippet: "fn main() {}".to_string(),
            },
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
    fn refresh_transient_error_preserves_existing_entry_and_mtime() {
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

        assert!(message.starts_with("ONNX Runtime not found. Install via:"));
        assert!(message.contains("Original error:"));
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
            max_batch_size: 64,
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
            max_batch_size: 64,
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
            max_batch_size: 64,
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
                kind: SymbolKind::Function,
                start_line: 10,
                end_line: 25,
                exported: true,
                embed_text: "file:src/main.rs kind:function name:handle_request".to_string(),
                snippet: "fn handle_request() {}".to_string(),
            },
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
        });
        index.write_to_disk(storage.path(), project_key);

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
                kind: SymbolKind::Function,
                start_line: 0,
                end_line: 0,
                exported: true,
                embed_text: "file:src/main.rs kind:function name:handle_request".to_string(),
                snippet: "fn handle_request() {}".to_string(),
            },
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
        };
        index.set_fingerprint(fingerprint.clone());

        let mut bytes = index.to_bytes();
        bytes[0] = SEMANTIC_INDEX_VERSION_V3;
        fs::write(dir.join("semantic.bin"), bytes).unwrap();

        assert!(SemanticIndex::read_from_disk(
            storage.path(),
            project_key,
            &test_project_root(),
            false,
            Some(&fingerprint.as_string())
        )
        .is_none());
        assert!(!dir.join("semantic.bin").exists());
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
}
