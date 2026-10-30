use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::fs;
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use htmd::{
    element_handler::{HandlerResult, Handlers},
    Element, HtmlToMarkdown,
};
use markup5ever_rcdom::{Node, NodeData};
use reqwest::blocking::{Client, Response as HttpResponse};
use reqwest::header::{
    ACCEPT, ACCEPT_ENCODING, CONTENT_ENCODING, CONTENT_TYPE, LOCATION, USER_AGENT,
};
use reqwest::redirect::Policy;
use secret_rules::audit::{AuditReason, AuditRecordV1, Decision};
use secret_rules::location::{
    evaluate_request_with_connect, location_kind_token, LabelOutcome, LabelRule, Location,
    RequestView,
};
use secret_rules::material::{check_admissible, encode_for_location};
use secret_rules::scrub::{MatcherSet, ScrubError, Scrubber};
use secret_rules::target::{normalize_request_target, ConnectAuthority, RequestTarget, RuleTarget};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;
use zeroize::Zeroizing;

use crate::parser::detect_language;

const MAX_RESPONSE_BYTES: u64 = 8 * 1024 * 1024;
const CACHE_TTL_MS: u64 = 24 * 60 * 60 * 1000;
/// Longest a request may take to set up its connection (name lookup, TCP
/// and TLS), within the fetch deadline.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(10_000);
/// Longest a request may wait for the response headers, counted from the
/// start of the request, within the fetch deadline. reqwest applies the same
/// limit to each later read of the body.
const HEADERS_TIMEOUT: Duration = Duration::from_millis(15_000);
const BODY_CHUNK_TIMEOUT: Duration = Duration::from_millis(15_000);
const MAX_REDIRECTS: usize = 5;

/// Longest one URL fetch may take, whatever budget the call has left. Every
/// phase (name lookup, connect, response headers, body) shares this one
/// deadline, so redirects and retries cannot multiply it.
const URL_FETCH_CEILING: Duration = Duration::from_secs(20);
/// The last part of the call's budget, kept for building and sending the
/// reply. Neither the download nor reading the downloaded document back may
/// use it, so the reply reaches the caller before the call's own deadline.
pub(crate) const URL_REPLY_MARGIN: Duration = Duration::from_secs(1);
/// The part of the call's budget before the reply margin that the download
/// leaves for the work after it: converting HTML, writing the cache, and
/// reading the document back. A download that ends at its deadline still has
/// this much time for that work.
const URL_READ_BACK_RESERVE: Duration = Duration::from_secs(2);
/// The part of the read-back reserve a placeholder fetch's final residual
/// key check may use, counted from the end of the download deadline. The
/// rest stays for writing and reading the document back.
const RESIDUAL_CHECK_RESERVE: Duration = Duration::from_millis(1500);
/// A transient failure is retried only when the backoff plus at least this
/// much time for the next attempt still fit before the fetch deadline.
const MIN_USEFUL_ATTEMPT: Duration = Duration::from_secs(1);

/// Retry budget for transient connect/transport failures only. Agents
/// shouldn't have to retry manually for a single TCP/TLS hiccup. We cap
/// at 2 retries (= 3 total attempts) with short jittered backoff so a
/// genuinely-broken host fails fast instead of dragging the foreground
/// fetch out to many seconds.
///
/// We deliberately do NOT retry on:
///   - HTTP error status (4xx/5xx) — the server actually answered
///   - Redirect errors / SSRF rejections — those are deterministic
///   - Body read stalls — already handled by BODY_CHUNK_TIMEOUT
const TRANSIENT_RETRY_ATTEMPTS: usize = 2;
const TRANSIENT_RETRY_BACKOFFS_MS: [u64; TRANSIENT_RETRY_ATTEMPTS] = [200, 600];
const ACCEPT_HEADER: &str = "application/vnd.github.raw, text/markdown, text/x-markdown, text/html;q=0.9, application/json;q=0.8, text/plain;q=0.5";
const USER_AGENT_VALUE: &str = "aft-opencode-plugin";
const CONVERTED_MARKDOWN_CONTENT_TYPE: &str = "text/markdown; charset=utf-8";

#[derive(Clone, Default)]
pub struct UrlFetchOptions {
    pub allow_private: bool,
    /// Test hook: treat a hostname as resolving to these IPs during SSRF validation.
    /// Production callers leave this empty and use `std::net::ToSocketAddrs`.
    #[doc(hidden)]
    pub public_host_overrides: Vec<(String, Vec<IpAddr>)>,
    /// Test hook: force reqwest to connect a hostname to a local mock server while
    /// SSRF validation still sees `public_host_overrides` above.
    #[doc(hidden)]
    pub connect_overrides: Vec<(String, SocketAddr)>,
    /// Test hook: observes the temp path immediately before the atomic rename.
    #[doc(hidden)]
    pub atomic_write_observer: Option<Arc<dyn Fn(&Path, &Path) + Send + Sync>>,
    /// Test hook: answers host name lookups in place of the system resolver,
    /// so a test can stall or redirect name resolution without a network.
    #[doc(hidden)]
    pub lookup_override: Option<HostLookup>,
    /// Test hook: holds every connection attempt this long before it starts
    /// connecting, standing in for a host that does not answer the connect.
    #[doc(hidden)]
    pub connect_delay: Option<Duration>,
    /// Test hook: the TLS configuration to use instead of the platform
    /// verifier, so a test can trust a local HTTPS server's certificate.
    #[doc(hidden)]
    pub tls_config: Option<rustls::ClientConfig>,
}

/// A host name lookup: the host's addresses, or the lookup's error.
pub type HostLookup = Arc<dyn Fn(&str) -> io::Result<Vec<IpAddr>> + Send + Sync>;

/// The part of a fetch that was running when its deadline or a stall limit
/// stopped it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchPhase {
    Dns,
    Connect,
    Headers,
    Body,
}

impl FetchPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dns => "dns",
            Self::Connect => "connect",
            Self::Headers => "headers",
            Self::Body => "body",
        }
    }
}

#[derive(Debug, Clone)]
pub struct UrlFetchError {
    code: &'static str,
    message: String,
    phase: Option<FetchPhase>,
    /// HTTP status of a server that answered with an error.
    status: Option<u16>,
    /// Set on a remembered failure: how long until a call fetches again.
    retry_after: Option<Duration>,
    /// Set when the fetch stopped (or never started) only because the
    /// calling tool call had too little budget left. Such a failure says
    /// nothing about the server, so it is never remembered.
    cut_by_call_budget: bool,
}

impl UrlFetchError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            code: "url_fetch_failed",
            message: message.into(),
            phase: None,
            status: None,
            retry_after: None,
            cut_by_call_budget: false,
        }
    }

    fn timeout(phase: FetchPhase, message: impl Into<String>) -> Self {
        Self {
            code: "url_fetch_timeout",
            phase: Some(phase),
            ..Self::new(message)
        }
    }

    /// A placeholder fetch refused by its secret rules or the vault, with the
    /// secret reply code (`secret_refused`, `secrets_unavailable`, ...).
    fn secret(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            ..Self::new(message)
        }
    }

    /// This error reworded for a refusal that came after a request carrying a
    /// key was sent; any other error is returned unchanged.
    fn after_secret_sent(mut self) -> Self {
        if let Some(rest) = self.message.strip_prefix(SECRET_FETCH_REFUSED) {
            self.message = format!("{SECRET_FETCH_REFUSED_AFTER_SEND}{rest}");
        }
        self
    }

    fn http_status(status: u16, message: impl Into<String>) -> Self {
        Self {
            status: Some(status),
            ..Self::new(message)
        }
    }

    /// Response error code: `url_fetch_timeout` when a deadline or stall
    /// limit stopped the fetch, otherwise `url_fetch_failed`.
    pub fn code(&self) -> &'static str {
        self.code
    }

    /// The phase a timed-out fetch was in; `None` for other failures.
    pub fn phase(&self) -> Option<FetchPhase> {
        self.phase
    }

    /// For a failure repeated from memory instead of a new fetch, the time
    /// until a call for the same URL fetches it again.
    pub fn retry_after(&self) -> Option<Duration> {
        self.retry_after
    }

    /// Failures that would very likely repeat on an immediate retry, and
    /// cost the retry the same wait: timeouts, server errors (5xx), and
    /// client errors (4xx) other than 408 Request Timeout and 429 Too Many
    /// Requests. Refusals by the SSRF check and other local errors are cheap
    /// to reproduce and are not remembered.
    ///
    /// Only failures the server or network caused are remembered: a timeout
    /// cut short by the caller's budget is not, and neither is a failure
    /// that is itself being repeated from memory.
    fn worth_remembering(&self) -> bool {
        if self.cut_by_call_budget || self.retry_after.is_some() {
            return false;
        }
        self.code == "url_fetch_timeout"
            || self
                .status
                .is_some_and(|status| status >= 400 && status != 408 && status != 429)
    }
}

/// One deadline for a whole fetch, taken from the call's filesystem budget
/// (less the read-back reserve and the reply margin) and capped at
/// [`URL_FETCH_CEILING`]. A call with no budget gets the ceiling.
#[derive(Debug, Clone, Copy)]
struct FetchDeadline {
    at: Instant,
    allowed: Duration,
    /// The call's budget, not the URL ceiling, set this deadline: a fetch it
    /// stops was stopped by the caller, not by a slow server.
    set_by_call: bool,
}

impl FetchDeadline {
    fn for_current_call() -> Self {
        let now = Instant::now();
        let ceiling = now + URL_FETCH_CEILING;
        let at = match crate::bounded_io::current() {
            Some(budget) => budget
                .deadline
                .checked_sub(URL_REPLY_MARGIN + URL_READ_BACK_RESERVE)
                .unwrap_or(now)
                .clamp(now, ceiling),
            None => ceiling,
        };
        Self {
            at,
            allowed: at.saturating_duration_since(now),
            set_by_call: at < ceiling,
        }
    }

    fn remaining(&self) -> Duration {
        self.at.saturating_duration_since(Instant::now())
    }

    fn expired(&self) -> bool {
        self.remaining().is_zero()
    }

    fn timeout_error(&self, phase: FetchPhase, url: &str) -> UrlFetchError {
        UrlFetchError {
            cut_by_call_budget: self.set_by_call,
            ..UrlFetchError::timeout(
                phase,
                format!(
                    "URL fetch timed out in the {} phase: the {}ms fetch deadline elapsed fetching {url}",
                    phase.as_str(),
                    self.allowed.as_millis()
                ),
            )
        }
    }
}

impl fmt::Display for UrlFetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for UrlFetchError {}

#[derive(Debug, Serialize, Deserialize)]
struct CacheMeta {
    url: String,
    #[serde(rename = "contentType")]
    content_type: String,
    extension: String,
    #[serde(rename = "fetchedAt")]
    fetched_at: u64,
    #[serde(default)]
    download_truncated: bool,
    #[serde(default)]
    downloaded_bytes: usize,
    #[serde(default)]
    total_bytes: Option<u64>,
}

pub fn is_http_url(value: &str) -> bool {
    value.starts_with("http://") || value.starts_with("https://")
}

/// Fetch `url` into the URL cache under `storage_dir` and return the cached
/// document's path. A failure worth remembering (see
/// [`UrlFetchError::worth_remembering`]) is repeated for
/// [`FAILURE_MEMORY`] instead of a new fetch, so an agent that retries a
/// page the server failed gets its answer at once instead of waiting again.
/// A document already in the cache is served regardless.
pub fn fetch_url_to_cache(
    url: &str,
    storage_dir: &Path,
    options: UrlFetchOptions,
) -> Result<PathBuf, UrlFetchError> {
    let key = FailureKey::new(storage_dir, url, options.allow_private);
    let result = download_to_cache(url, storage_dir, options, &key);
    match &result {
        Ok(_) => forget_failure(&key),
        Err(error) if error.worth_remembering() => remember_failure(key, error),
        Err(_) => {}
    }
    result
}

/// How long a remembered failure stands in for a new fetch of its URL.
const FAILURE_MEMORY: Duration = Duration::from_secs(60);

/// A remembered failure is specific to the cache it would have filled and to
/// whether private addresses were allowed.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct FailureKey {
    storage_dir: PathBuf,
    url: String,
    allow_private: bool,
}

impl FailureKey {
    fn new(storage_dir: &Path, url: &str, allow_private: bool) -> Self {
        Self {
            storage_dir: storage_dir.to_path_buf(),
            url: url.to_string(),
            allow_private,
        }
    }
}

struct RememberedFailure {
    error: UrlFetchError,
    at: Instant,
}

/// Remembered failures. Expired entries are dropped on every insert, and a
/// lookup ignores an entry older than [`FAILURE_MEMORY`].
fn failure_memory() -> &'static std::sync::Mutex<HashMap<FailureKey, RememberedFailure>> {
    static FAILURES: std::sync::OnceLock<std::sync::Mutex<HashMap<FailureKey, RememberedFailure>>> =
        std::sync::OnceLock::new();
    FAILURES.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

fn remembered_failure(key: &FailureKey) -> Option<UrlFetchError> {
    let failures = failure_memory()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let remembered = failures.get(key)?;
    let age = remembered.at.elapsed();
    let retry_after = FAILURE_MEMORY
        .checked_sub(age)
        .filter(|left| !left.is_zero())?;
    let mut error = remembered.error.clone();
    error.message = format!(
        "{} (cached failure from {}s ago; this URL is fetched again after {}s)",
        error.message,
        age.as_secs(),
        retry_after.as_secs_f64().ceil() as u64
    );
    error.retry_after = Some(retry_after);
    Some(error)
}

fn remember_failure(key: FailureKey, error: &UrlFetchError) {
    let mut failures = failure_memory()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    failures.retain(|_, remembered| remembered.at.elapsed() < FAILURE_MEMORY);
    failures.insert(
        key,
        RememberedFailure {
            error: error.clone(),
            at: Instant::now(),
        },
    );
}

fn forget_failure(key: &FailureKey) {
    failure_memory()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(key);
}

/// Test hook: a document-cache read under `storage_dir` that blocks until
/// released (or for at most 10 s, so a broken test cannot hang the suite),
/// standing in for a cache filesystem that stops answering.
#[cfg(test)]
#[derive(Default)]
struct CacheReadStall {
    reached: std::sync::atomic::AtomicUsize,
    released: std::sync::Mutex<bool>,
    release: std::sync::Condvar,
}

#[cfg(test)]
impl CacheReadStall {
    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.release.notify_all();
    }
}

#[cfg(test)]
fn cache_read_stalls() -> &'static std::sync::Mutex<HashMap<PathBuf, Arc<CacheReadStall>>> {
    static STALLS: std::sync::OnceLock<std::sync::Mutex<HashMap<PathBuf, Arc<CacheReadStall>>>> =
        std::sync::OnceLock::new();
    STALLS.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

#[cfg(test)]
fn stall_cache_reads(storage_dir: &Path) -> Arc<CacheReadStall> {
    let stall = Arc::new(CacheReadStall::default());
    cache_read_stalls()
        .lock()
        .unwrap()
        .insert(storage_dir.to_path_buf(), Arc::clone(&stall));
    stall
}

#[cfg(test)]
fn stall_cache_read_if_requested(storage_dir: &Path) {
    let Some(stall) = cache_read_stalls()
        .lock()
        .unwrap()
        .get(storage_dir)
        .cloned()
    else {
        return;
    };
    stall
        .reached
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let released = stall.released.lock().unwrap();
    let _ = stall
        .release
        .wait_timeout_while(released, Duration::from_secs(10), |released| !*released);
}

/// Test hook: make one remembered failure `by` older.
#[cfg(test)]
fn age_remembered_failure(key: &FailureKey, by: Duration) {
    if let Some(remembered) = failure_memory()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get_mut(key)
    {
        remembered.at = remembered
            .at
            .checked_sub(by)
            .expect("clock far enough along");
    }
}

/// What a document-cache check needs, owned so a helper thread can run it.
struct CacheCheckInput {
    storage_dir: PathBuf,
    hash: String,
    meta_file: PathBuf,
    url: Url,
}

type CacheCheckResult = Result<Option<PathBuf>, UrlFetchError>;

/// A document-cache check running on a helper thread. Calls that find one
/// running for their URL wait for it instead of starting another, so a cache
/// filesystem that stops answering holds at most one thread per URL.
#[derive(Default)]
struct CacheCheck {
    result: std::sync::Mutex<Option<CacheCheckResult>>,
    done: std::sync::Condvar,
}

fn cache_checks() -> &'static std::sync::Mutex<HashMap<FailureKey, Arc<CacheCheck>>> {
    static CHECKS: std::sync::OnceLock<std::sync::Mutex<HashMap<FailureKey, Arc<CacheCheck>>>> =
        std::sync::OnceLock::new();
    CHECKS.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

/// The latest a bounded cache check may answer: the call's deadline less the
/// reply margin, or the URL ceiling for a call without a budget.
fn cache_check_deadline() -> Instant {
    let now = Instant::now();
    let ceiling = now + URL_FETCH_CEILING;
    match crate::bounded_io::current() {
        Some(budget) => budget
            .deadline
            .checked_sub(URL_REPLY_MARGIN)
            .unwrap_or(now)
            .clamp(now, ceiling),
        None => ceiling,
    }
}

/// Run (or join) the document-cache check for `key` and wait for it until
/// `deadline`. `Ok(None)` means no usable cached copy, including a check
/// that did not finish in time.
fn bounded_cache_check(
    key: &FailureKey,
    input: CacheCheckInput,
    deadline: Instant,
) -> CacheCheckResult {
    let check = {
        let mut checks = cache_checks()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match checks.get(key) {
            Some(running) => Arc::clone(running),
            None => {
                let check = Arc::new(CacheCheck::default());
                checks.insert(key.clone(), Arc::clone(&check));
                let worker = Arc::clone(&check);
                let worker_key = key.clone();
                thread::spawn(move || {
                    let result = fresh_cached_path(
                        &input.storage_dir,
                        &input.hash,
                        &input.meta_file,
                        &input.url,
                    );
                    *worker
                        .result
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(result);
                    worker.done.notify_all();
                    cache_checks()
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .remove(&worker_key);
                });
                check
            }
        }
    };
    let mut result = check
        .result
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    loop {
        if let Some(result) = result.as_ref() {
            return result.clone();
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(None);
        }
        result = check
            .done
            .wait_timeout(result, left)
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .0;
    }
}

/// A document that is cached is served (after the SSRF check) even while a
/// failure of the same URL is remembered; only a cache miss is answered by a
/// remembered failure. With a failure remembered, the call waits for the
/// cache check only until its deadline less the reply margin; a check still
/// running then counts as a miss, and its helper thread may run on.
fn download_to_cache(
    url: &str,
    storage_dir: &Path,
    options: UrlFetchOptions,
    failure_key: &FailureKey,
) -> Result<PathBuf, UrlFetchError> {
    let parsed = Url::parse(url).map_err(|_| UrlFetchError::new(format!("Invalid URL: {url}")))?;
    let deadline = FetchDeadline::for_current_call();
    let fetch_url = rewrite_url_for_fetch(&parsed);
    let approved = Arc::new(ApprovedHosts::default());
    let hash = hash_url(url);
    let meta_file = meta_path(storage_dir, &hash);

    // With a failure remembered, the answer is already in memory, so the call
    // must not wait on the cache check past its deadline; the check runs on
    // a helper thread that may outlive the call. No directory is created:
    // nothing will be written.
    if let Some(remembered) = remembered_failure(failure_key) {
        let check = CacheCheckInput {
            storage_dir: storage_dir.to_path_buf(),
            hash,
            meta_file,
            url: fetch_url.clone(),
        };
        let Some(cached) = bounded_cache_check(failure_key, check, cache_check_deadline())? else {
            return Err(remembered);
        };
        validate_public_url(&fetch_url, &options, &deadline, &approved)?;
        return Ok(cached);
    }

    let dir = cache_dir(storage_dir);
    crate::private_storage::open_dir(storage_dir, &dir).map_err(|error| {
        UrlFetchError::new(format!(
            "Failed to create URL cache directory {}: {error}",
            dir.display()
        ))
    })?;

    let cached = fresh_cached_path(storage_dir, &hash, &meta_file, &fetch_url)?;
    // A cache hit is checked too: the request may refuse private addresses
    // that the call which filled the cache allowed.
    validate_public_url(&fetch_url, &options, &deadline, &approved)?;
    if let Some(cached) = cached {
        return Ok(cached);
    }

    let response = fetch_with_redirects(&fetch_url, url, &options, &deadline, approved)?;
    if !response.status().is_success() {
        return Err(UrlFetchError::http_status(
            response.status().as_u16(),
            format!(
                "HTTP {} {} fetching {url}",
                response.status().as_u16(),
                response.status().canonical_reason().unwrap_or("")
            ),
        ));
    }

    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("text/plain")
        .to_string();
    let (extension, from_source_path) =
        resolve_fetch_extension(&fetch_url, &content_type).ok_or_else(|| {
            UrlFetchError::new(format!(
                "Unsupported content type '{content_type}' for {url}. Supported: text/html, text/markdown, application/json, text/plain; source files via URL path extension (e.g. .rs, .ts, .mjs)"
            ))
        })?;

    let total_bytes = response.content_length();
    let (mut body, download_truncated) = read_response_body(response, url, &deadline)?;
    let downloaded_bytes = body.len();
    // A byte ceiling can bisect the final UTF-8 character. Do not turn an
    // otherwise valid document prefix into an unreadable cache entry.
    if download_truncated {
        if let Err(error) = std::str::from_utf8(&body) {
            if error.error_len().is_none() {
                body.truncate(error.valid_up_to());
            }
        }
    }
    if from_source_path && body_contains_nul_in_prefix(&body) {
        return Err(UrlFetchError::new(format!(
            "Binary content detected for source URL {url}"
        )));
    }
    let (body, content_type, extension) = if extension == ".html" {
        (
            convert_html_body_to_markdown(&body, url)?,
            CONVERTED_MARKDOWN_CONTENT_TYPE.to_string(),
            ".md",
        )
    } else {
        (body, content_type, extension)
    };

    let content_file = content_path(storage_dir, &hash, extension);
    atomic_write(&content_file, &body, &options)?;

    let meta = CacheMeta {
        url: url.to_string(),
        content_type,
        extension: extension.to_string(),
        fetched_at: now_ms(),
        download_truncated,
        downloaded_bytes,
        total_bytes,
    };
    let meta_bytes = serde_json::to_vec(&meta).map_err(|error| {
        UrlFetchError::new(format!("Failed to encode URL cache metadata: {error}"))
    })?;
    atomic_write(&meta_file, &meta_bytes, &options)?;

    Ok(content_file)
}

/// The fresh cached copy of `url` under `storage_dir`, if there is one. Reads
/// only: it creates no folder, writes nothing and downloads nothing. The cache
/// check is bounded by the call's deadline like a remembered failure's (a
/// stalled cache drive counts as a miss), and a cached copy passes the same
/// scheme and private-address checks as a download before it is served.
pub fn cached_url_document(
    url: &str,
    storage_dir: &Path,
    options: &UrlFetchOptions,
) -> Result<Option<PathBuf>, UrlFetchError> {
    let parsed = Url::parse(url).map_err(|_| UrlFetchError::new(format!("Invalid URL: {url}")))?;
    let deadline = FetchDeadline::for_current_call();
    let fetch_url = rewrite_url_for_fetch(&parsed);
    let hash = hash_url(url);
    let check = CacheCheckInput {
        storage_dir: storage_dir.to_path_buf(),
        meta_file: meta_path(storage_dir, &hash),
        hash,
        url: fetch_url.clone(),
    };
    let key = FailureKey::new(storage_dir, url, options.allow_private);
    let Some(cached) = bounded_cache_check(&key, check, cache_check_deadline())? else {
        return Ok(None);
    };
    validate_public_url(&fetch_url, options, &deadline, &ApprovedHosts::default())?;
    Ok(Some(cached))
}

/// Folder, under the cache root, that holds the private per-call folders of
/// placeholder fetches. It is a sibling of the URL cache, never inside it.
pub(crate) const SECRET_FETCH_DIR: &str = "url_secret_fetch";

/// The start of every refusal a placeholder fetch makes on its own (as opposed
/// to a refusal the vault decided) before any request carrying a key was sent.
/// One constant so the wording is reviewed once; the message goes on to name a
/// stable reason token and the URL as the agent wrote it, with the
/// placeholder and never a key.
pub(crate) const SECRET_FETCH_REFUSED: &str =
    "Secret placeholder fetch refused; no request carrying the secret was sent";

/// The start of the same refusals once a request carrying a key has been sent:
/// the response (or a later hop) was refused, so the claim above would be
/// false.
pub(crate) const SECRET_FETCH_REFUSED_AFTER_SEND: &str =
    "Secret placeholder fetch stopped after a request carrying the secret was sent; its response was not used";

/// A stand-in key used only to plan where a placeholder fetch would
/// substitute. Real vault bytes are asked for only once a rule matches.
const SECRET_PLANNING_KEY: &[u8] = b"url-fetch-planning-value-0000";

/// The most content codings a placeholder fetch decodes from one response.
const MAX_SECRET_CODINGS: usize = 4;

/// Every label of a well-formed placeholder in `url`, in order and without
/// repeats, or `None` when the URL mentions no placeholder at all.
///
/// A percent-encoded placeholder counts as well as a literal one, because a
/// query value is compared after decoding. `Some` with no labels means the URL
/// mentions the placeholder prefix only in a malformed way: such a URL still
/// takes the placeholder path (no cache, no web-view rewrite), it just has
/// nothing to substitute.
pub(crate) fn secret_labels_in_url(url: &str) -> Option<Vec<String>> {
    if !secret_rules::placeholder::contains_placeholder_prefix_loose(url) {
        return None;
    }
    let decoded = percent_decode_path_segment(url);
    let mut labels: Vec<String> = Vec::new();
    for text in [url, decoded.as_str()] {
        for placeholder in secret_rules::placeholder::scan(text) {
            if !labels.iter().any(|label| label == placeholder.label) {
                labels.push(placeholder.label.to_string());
            }
        }
    }
    Some(labels)
}

/// Create the private folder one placeholder fetch writes its document into,
/// under `storage_dir` but outside the URL cache. The caller owns the folder
/// and deletes it when its call returns.
pub(crate) fn create_secret_fetch_dir(storage_dir: &Path) -> Result<PathBuf, UrlFetchError> {
    let parent = storage_dir.join(SECRET_FETCH_DIR);
    let failed = |error: io::Error| {
        UrlFetchError::new(format!(
            "Failed to create a private folder for a placeholder fetch under {}: {error}",
            parent.display()
        ))
    };
    crate::private_storage::open_dir(storage_dir, &parent).map_err(failed)?;
    let dir = parent.join(random_nonce());
    // `create_dir` fails when the folder exists, so no other call shares it.
    crate::private_storage::create_dir(&dir).map_err(failed)?;
    Ok(dir)
}

/// What a placeholder fetch may do with its secrets.
pub struct SecretFetch<'a> {
    /// The vault `material` and `audit` go to.
    pub vault: &'a dyn crate::secret::SecretVault,
    /// The call's authorization scope; `None` only when the URL holds no
    /// well-formed placeholder, in which case nothing is resolved or audited.
    pub authorization: Option<crate::secret::SecretScope>,
    /// Every label found in the URL, in order. Each has a rule in `rules`.
    pub labels: Vec<String>,
    /// The rule the vault granted for each label, carrying its grant id.
    pub rules: Vec<LabelRule>,
    /// The fetch call's id: audit records name it, plus the hop index.
    pub call_id: String,
}

/// Keys the vault released during one placeholder fetch. They are kept, wiped
/// on drop, only until the fetch returns, so every header and body read after
/// a key was sent is scrubbed of it.
#[derive(Default)]
struct HeldKeys(Vec<(String, Zeroizing<Vec<u8>>)>);

impl HeldKeys {
    fn matchers(&self) -> Result<Vec<MatcherSet>, ScrubError> {
        self.0
            .iter()
            .map(|(label, key)| MatcherSet::new(label, key))
            .collect()
    }

    /// Hold `key` for `label`, refusing a key whose matcher would collide with
    /// one already held (the scrubber could then write one key into a reply).
    fn hold(&mut self, label: &str, key: &[u8]) -> Result<(), ScrubError> {
        if self
            .0
            .iter()
            .any(|(held, bytes)| held == label && bytes.as_slice() == key)
        {
            return Ok(());
        }
        let mut matchers = self.matchers()?;
        matchers.push(MatcherSet::new(label, key)?);
        Scrubber::new(matchers)?;
        self.0
            .push((label.to_string(), Zeroizing::new(key.to_vec())));
        Ok(())
    }

    /// `bytes` with every held key replaced by its label's placeholder.
    fn scrub(&self, bytes: &[u8]) -> Result<Vec<u8>, ScrubError> {
        let mut scrubber = Scrubber::new(self.matchers()?)?;
        let mut clean = scrubber.push(bytes);
        clean.extend_from_slice(&scrubber.finish());
        Ok(clean)
    }

    fn keys(&self) -> Vec<&[u8]> {
        self.0.iter().map(|(_, key)| key.as_slice()).collect()
    }

    /// Error text from a library (reqwest, the decoders) with every held key
    /// replaced by its placeholder. Text that still spells a key after that,
    /// or that cannot be scrubbed, is dropped whole.
    fn sanitize_detail(&self, detail: &str) -> String {
        let deadline = Instant::now() + DETAIL_CHECK_LIMIT;
        match self.scrub(detail.as_bytes()) {
            Ok(clean) if self.reveal(&clean, deadline).is_clean() => {
                String::from_utf8_lossy(&clean).into_owned()
            }
            _ => String::new(),
        }
    }

    /// Whether `bytes` still spell a held key, as they stand or after a URL
    /// parser or reader normalizes them, checked until `deadline` (see
    /// [`secret_rules::reveal_outcome`]).
    fn reveal(&self, bytes: &[u8], deadline: Instant) -> secret_rules::RevealOutcome {
        secret_rules::reveal_outcome(bytes, &self.keys(), Some(deadline))
    }
}

/// The longest the residual check of a library error's text may run; such
/// text is short, and text the check cannot clear in time is dropped.
const DETAIL_CHECK_LIMIT: Duration = Duration::from_millis(100);

/// Refuse unless the residual check found `bytes` clean: `found` names the
/// refusal when a key was spelled, and a check that could not finish refuses
/// as exhausted or timed out.
fn refuse_unless_clean(
    outcome: secret_rules::RevealOutcome,
    found: &str,
    url: &str,
) -> Result<(), UrlFetchError> {
    match outcome {
        secret_rules::RevealOutcome::Clean => Ok(()),
        secret_rules::RevealOutcome::Found => Err(secret_fetch_refused(found, url)),
        secret_rules::RevealOutcome::Exhausted => {
            Err(secret_fetch_refused("residual_check_exhausted", url))
        }
        secret_rules::RevealOutcome::OutOfTime => {
            Err(secret_fetch_refused("residual_check_timeout", url))
        }
    }
}

fn secret_fetch_refused(reason: &str, url: &str) -> UrlFetchError {
    UrlFetchError::secret(
        crate::secret::SECRET_REFUSED,
        format!("{SECRET_FETCH_REFUSED} ({reason}): {url}"),
    )
}

fn scrub_refusal(url: &str) -> impl Fn(ScrubError) -> UrlFetchError + '_ {
    move |_| secret_fetch_refused("scrubber_unavailable", url)
}

/// Fetch `logical_url`, a URL that mentions a secret placeholder, into
/// `document_dir` and return the converted document's path.
///
/// The URL cache is never read or written, and the web-view rewrite is
/// skipped, so the URL the vault's rule is matched against is the one the
/// agent gave. Every hop, redirects included, is checked against the private
/// address policy and then evaluated from scratch against the granted rules:
/// only a hop a rule covers gets a copy of the URL with the key in its query.
/// Every response header this function reads (`Location` first of all) and
/// the body are scrubbed of each key released so far before they are used, and
/// every message names `logical_url` (or a scrubbed redirect target), never a
/// URL that held a key.
pub fn fetch_url_with_secrets(
    logical_url: &str,
    document_dir: &Path,
    options: UrlFetchOptions,
    secrets: &SecretFetch<'_>,
) -> Result<PathBuf, UrlFetchError> {
    let key_sent = std::cell::Cell::new(false);
    fetch_with_secrets(logical_url, document_dir, options, secrets, &key_sent).map_err(|error| {
        // A refusal written as "nothing was sent" is reworded once a request
        // carrying the key has gone out.
        if key_sent.get() {
            error.after_secret_sent()
        } else {
            error
        }
    })
}

/// [`fetch_url_with_secrets`], recording in `key_sent` when a request that
/// carried a key has been sent (or attempted).
fn fetch_with_secrets(
    logical_url: &str,
    document_dir: &Path,
    options: UrlFetchOptions,
    secrets: &SecretFetch<'_>,
    key_sent: &std::cell::Cell<bool>,
) -> Result<PathBuf, UrlFetchError> {
    let mut current = Url::parse(logical_url)
        .map_err(|_| UrlFetchError::new(format!("Invalid URL: {logical_url}")))?;
    // A fragment never reaches a server, and a rule never matches one.
    current.set_fragment(None);
    let start = current.clone();
    let deadline = FetchDeadline::for_current_call();
    let approved = Arc::new(ApprovedHosts::default());
    let progress = Arc::new(ConnectProgress::default());
    let client = build_client(&options, Arc::clone(&approved), Arc::clone(&progress))?;
    let mut held = HeldKeys::default();
    let mut hop = 0;
    let response = loop {
        validate_public_url(&current, &options, &deadline, &approved)?;
        let outbound = secret_hop(&current, hop, secrets, &mut held)?;
        if outbound.is_some() {
            key_sent.set(true);
        }
        let sent = send_request(
            &client,
            outbound.as_ref().unwrap_or(&current),
            Some((current.as_str(), &held)),
            &deadline,
            &progress,
        );
        // Wipe this function's copy of the key-bearing URL now that it is sent.
        drop(outbound.map(|url| Zeroizing::new(String::from(url))));
        let response = sent?;
        if !response.status().is_redirection() {
            break response;
        }
        if hop == MAX_REDIRECTS {
            return Err(UrlFetchError::new(format!(
                "Too many redirects fetching {logical_url}"
            )));
        }
        // A control byte never belongs in a Location, and URL parsing would
        // drop some of them (a tab), joining text the scrubber saw apart.
        if response.headers().get_all(LOCATION).iter().any(|value| {
            value
                .as_bytes()
                .iter()
                .any(|byte| *byte < 0x20 || *byte == 0x7f)
        }) {
            return Err(secret_fetch_refused("redirect_control_byte", logical_url));
        }
        let location = scrubbed_header(&response, LOCATION, &held, logical_url, &deadline)?
            .filter(|value| value.bytes().all(|byte| (0x20..0x7f).contains(&byte)))
            .ok_or_else(|| {
                UrlFetchError::new(format!(
                    "Redirect from {} missing Location header",
                    current.as_str()
                ))
            })?;
        let mut next = current.join(&location).map_err(|error| {
            UrlFetchError::new(format!(
                "Invalid redirect Location '{location}' from {}: {error}",
                current.as_str()
            ))
        })?;
        next.set_fragment(None);
        // Joining normalizes the URL (dot segments collapse, the host is
        // lowercased, escapes are kept), which can reassemble a key the
        // header scrub did not see. Check the URL exactly as it will be
        // looked up, sent and named, before any of that happens.
        refuse_unless_clean(
            held.reveal(next.as_str().as_bytes(), header_check_deadline(&deadline)),
            "redirect_discloses_key",
            logical_url,
        )?;
        current = next;
        hop += 1;
    };

    let status = response.status();
    if !status.is_success() {
        return Err(UrlFetchError::http_status(
            status.as_u16(),
            format!(
                "HTTP {} {} fetching {logical_url}",
                status.as_u16(),
                status.canonical_reason().unwrap_or("")
            ),
        ));
    }
    let content_type = scrubbed_header(&response, CONTENT_TYPE, &held, logical_url, &deadline)?
        .unwrap_or_else(|| "text/plain".to_string());
    // `HeaderMap::get` reads only the first field. A second Content-Encoding
    // field could name a coding behind an `identity` first one, so repeated
    // fields refuse the response rather than being read in part.
    if response.headers().get_all(CONTENT_ENCODING).iter().count() > 1 {
        return Err(secret_fetch_refused("content_encoding", logical_url));
    }
    let codings = match scrubbed_header(&response, CONTENT_ENCODING, &held, logical_url, &deadline)?
    {
        Some(value) => secret_content_codings(&value, logical_url)?,
        None if response.headers().contains_key(CONTENT_ENCODING) => {
            return Err(secret_fetch_refused("content_encoding", logical_url));
        }
        None => Vec::new(),
    };
    let (extension, from_source_path) = resolve_fetch_extension(&start, &content_type)
        .ok_or_else(|| {
            UrlFetchError::new(format!(
                "Unsupported content type '{content_type}' for {logical_url}. Supported: text/html, text/markdown, application/json, text/plain; source files via URL path extension (e.g. .rs, .ts, .mjs)"
            ))
        })?;
    // An HTML response is converted whatever its path says (an index.php
    // page, or a .json endpoint answering with an HTML error page). Kept raw,
    // its markup would hide a key split across tags or spelled with entities
    // from the scrub while any browser shows it; converted, the residual
    // check below sees the text a reader sees.
    let is_html =
        extension == ".html" || resolve_extension_from_content_type(&content_type) == Some(".html");
    let total_bytes = codings
        .is_empty()
        .then(|| response.content_length())
        .flatten();
    // A failed read's text comes from reqwest and may name the request URL,
    // which held the key; it is scrubbed like everything else read back.
    let sanitize = |detail: &str| held.sanitize_detail(detail);
    #[cfg(test)]
    let response: Box<dyn Read + Send> =
        match TEST_BODY_READ_ERROR.with(|slot| slot.borrow().clone()) {
            Some(message) => Box::new(FailingBody(message)),
            None => Box::new(response),
        };
    let (raw, mut download_truncated) =
        read_response_body_with(response, logical_url, &deadline, &sanitize)?;
    let raw = Zeroizing::new(raw);
    let decoded = decode_secret_body(&raw, &codings, &mut download_truncated, logical_url)?;
    let mut body = held.scrub(&decoded).map_err(scrub_refusal(logical_url))?;
    drop(decoded);
    let downloaded_bytes = body.len();
    if download_truncated {
        if let Err(error) = std::str::from_utf8(&body) {
            if error.error_len().is_none() {
                body.truncate(error.valid_up_to());
            }
        }
    }
    if from_source_path && !is_html && body_contains_nul_in_prefix(&body) {
        return Err(UrlFetchError::new(format!(
            "Binary content detected for source URL {logical_url}"
        )));
    }
    let (body, content_type, extension) = if is_html {
        // Converting HTML can decode entities into new text, so the converted
        // document is scrubbed again before it is written.
        let markdown = convert_html_body_to_markdown(&body, logical_url)?;
        (
            held.scrub(&markdown).map_err(scrub_refusal(logical_url))?,
            CONVERTED_MARKDOWN_CONTENT_TYPE.to_string(),
            ".md",
        )
    } else {
        (body, content_type, extension)
    };

    // The scrub replaced every spelling it recognised, but conversion and
    // plain readers undo more (a Markdown-escaped `\*`, an HTML entity kept in
    // a page served under a source path). A document that still spells a held
    // key after those is refused, never written; so is one the check could
    // not finish, within its work budget or before its share of the call's
    // read-back reserve ran out.
    refuse_unless_clean(
        held.reveal(&body, residual_check_deadline(&deadline)),
        "document_discloses_key",
        logical_url,
    )?;
    let content_file = document_dir.join(format!("document{extension}"));
    write_secret_document(&content_file, &body)?;
    // The metadata sits where `download_notice` looks for it, so a truncated
    // download is disclosed exactly as a cached one is.
    let meta = CacheMeta {
        url: logical_url.to_string(),
        content_type,
        extension: extension.to_string(),
        fetched_at: now_ms(),
        download_truncated,
        downloaded_bytes,
        total_bytes,
    };
    let meta_bytes = serde_json::to_vec(&meta).map_err(|error| {
        UrlFetchError::new(format!("Failed to encode URL fetch metadata: {error}"))
    })?;
    write_secret_document(&content_file.with_extension("meta.json"), &meta_bytes)?;
    Ok(content_file)
}

fn write_secret_document(path: &Path, bytes: &[u8]) -> Result<(), UrlFetchError> {
    crate::private_storage::write(path, bytes).map_err(|error| {
        UrlFetchError::new(format!(
            "Failed to write placeholder fetch document {}: {error}",
            path.display()
        ))
    })
}

/// A response header's value, scrubbed of every held key before anything
/// reads it; `None` when the header is absent or not text.
fn scrubbed_header(
    response: &HttpResponse,
    name: reqwest::header::HeaderName,
    held: &HeldKeys,
    url: &str,
    deadline: &FetchDeadline,
) -> Result<Option<String>, UrlFetchError> {
    let Some(value) = response.headers().get(&name) else {
        return Ok(None);
    };
    let clean = held.scrub(value.as_bytes()).map_err(scrub_refusal(url))?;
    // The scrubbed value is read, joined or quoted next; a key it still
    // spells in a form the scrub missed refuses the response instead.
    refuse_unless_clean(
        held.reveal(&clean, header_check_deadline(deadline)),
        "header_discloses_key",
        url,
    )?;
    Ok(String::from_utf8(clean).ok())
}

#[cfg(test)]
thread_local! {
    /// When set, a placeholder fetch's body read fails with this text, so a
    /// test can check what the call site does with a library error.
    static TEST_BODY_READ_ERROR: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// Make this thread's placeholder fetches fail their body read with
/// `message` until the returned guard drops.
#[cfg(test)]
pub(crate) fn inject_secret_body_read_error_for_test(message: String) -> impl Drop {
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            TEST_BODY_READ_ERROR.with(|slot| slot.replace(None));
        }
    }
    TEST_BODY_READ_ERROR.with(|slot| slot.replace(Some(message)));
    Restore
}

/// A response body whose every read fails with the injected text.
#[cfg(test)]
struct FailingBody(String);

#[cfg(test)]
impl Read for FailingBody {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        Err(io::Error::other(self.0.clone()))
    }
}

/// When a placeholder fetch's final residual key check must stop: its share
/// of the read-back reserve after the download deadline, or sooner in a test
/// that injects a shorter limit.
fn residual_check_deadline(deadline: &FetchDeadline) -> Instant {
    #[cfg(test)]
    if let Some(limit) = TEST_RESIDUAL_LIMIT.with(std::cell::Cell::get) {
        return Instant::now() + limit;
    }
    deadline.at + RESIDUAL_CHECK_RESERVE
}

/// When the residual check of a response header or a joined redirect URL
/// must stop: the download deadline, since those checks run while the fetch
/// is still downloading, or sooner in a test that injects a shorter limit.
fn header_check_deadline(deadline: &FetchDeadline) -> Instant {
    #[cfg(test)]
    if let Some(limit) = TEST_RESIDUAL_LIMIT.with(std::cell::Cell::get) {
        return Instant::now() + limit;
    }
    deadline.at
}

#[cfg(test)]
thread_local! {
    /// When set, each of a placeholder fetch's residual checks (headers,
    /// redirect URLs, the document) stops this long after it starts, so a
    /// test can reach the timeout without a slow document.
    static TEST_RESIDUAL_LIMIT: std::cell::Cell<Option<Duration>> = const { std::cell::Cell::new(None) };
}

/// Make this thread's placeholder fetches stop their residual check `limit`
/// after it starts, until the returned guard drops.
#[cfg(test)]
pub(crate) fn limit_secret_residual_check_for_test(limit: Duration) -> impl Drop {
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            TEST_RESIDUAL_LIMIT.with(|slot| slot.set(None));
        }
    }
    TEST_RESIDUAL_LIMIT.with(|slot| slot.set(Some(limit)));
    Restore
}

/// A content coding a placeholder fetch can undo before scrubbing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SecretCoding {
    Identity,
    Gzip,
    Deflate,
    Brotli,
}

/// The codings a response declares, in the order they were applied. A server
/// may encode the body although the request asked for `identity`; any coding
/// this function cannot undo refuses the document, because an encoded key
/// cannot be scrubbed. A coding named twice refuses it too: no ordinary server
/// encodes a body twice the same way, and the limit on stacked codings stays
/// meaningful.
fn secret_content_codings(value: &str, url: &str) -> Result<Vec<SecretCoding>, UrlFetchError> {
    let mut codings = Vec::new();
    for token in value.split(',').map(str::trim) {
        let coding = if token.eq_ignore_ascii_case("identity") {
            SecretCoding::Identity
        } else if token.eq_ignore_ascii_case("gzip") || token.eq_ignore_ascii_case("x-gzip") {
            SecretCoding::Gzip
        } else if token.eq_ignore_ascii_case("deflate") {
            SecretCoding::Deflate
        } else if token.eq_ignore_ascii_case("br") {
            SecretCoding::Brotli
        } else {
            return Err(secret_fetch_refused("content_encoding", url));
        };
        if codings.len() >= MAX_SECRET_CODINGS || codings.contains(&coding) {
            return Err(secret_fetch_refused("content_encoding", url));
        }
        codings.push(coding);
    }
    Ok(codings)
}

/// Undo `codings` (last applied first), keeping at most
/// [`MAX_RESPONSE_BYTES`] of output. A body whose download was cut short keeps
/// what decoded before the cut; any other decoding failure refuses it.
fn decode_secret_body(
    body: &[u8],
    codings: &[SecretCoding],
    truncated: &mut bool,
    url: &str,
) -> Result<Zeroizing<Vec<u8>>, UrlFetchError> {
    use async_compression::tokio::bufread::{BrotliDecoder, GzipDecoder, ZlibDecoder};
    use tokio::io::{AsyncRead, BufReader};

    let mut data = Zeroizing::new(body.to_vec());
    for coding in codings.iter().rev() {
        let input: &[u8] = &data;
        let reader: std::pin::Pin<Box<dyn AsyncRead + Send + '_>> = match coding {
            SecretCoding::Identity => continue,
            SecretCoding::Gzip => {
                let mut decoder = GzipDecoder::new(BufReader::new(input));
                decoder.multiple_members(true);
                Box::pin(decoder)
            }
            SecretCoding::Deflate => Box::pin(ZlibDecoder::new(BufReader::new(input))),
            SecretCoding::Brotli => Box::pin(BrotliDecoder::new(BufReader::new(input))),
        };
        let input_truncated = *truncated;
        let (output, cut) = drain_decoder(reader, input_truncated).map_err(|()| {
            UrlFetchError::new(format!("Failed to decode the response body for {url}"))
        })?;
        if cut {
            *truncated = true;
        }
        data = output;
    }
    Ok(data)
}

/// Read a decoder over an in-memory body to its end, keeping at most
/// [`MAX_RESPONSE_BYTES`]. Returns the output and whether the ceiling cut it.
/// A body whose download was already cut (`input_truncated`) ends mid-stream;
/// what decoded before the cut is kept as a usable prefix.
fn drain_decoder(
    mut reader: std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send + '_>>,
    input_truncated: bool,
) -> Result<(Zeroizing<Vec<u8>>, bool), ()> {
    use std::future::Future;
    use tokio::io::AsyncReadExt;

    let mut output = Zeroizing::new(Vec::new());
    let polled = {
        let decoding = async {
            let mut buffer = Zeroizing::new(vec![0u8; 16 * 1024]);
            loop {
                match reader.read(&mut buffer).await {
                    Ok(0) => return Ok(false),
                    Ok(count) => {
                        let room = (MAX_RESPONSE_BYTES as usize).saturating_sub(output.len());
                        output.extend_from_slice(&buffer[..count.min(room)]);
                        if count > room {
                            return Ok(true);
                        }
                    }
                    Err(_) if input_truncated => return Ok(false),
                    Err(_) => return Err(()),
                }
            }
        };
        // The input is already in memory, so no read ever waits: one poll
        // runs the decoder to its end without an async runtime.
        let mut decoding = std::pin::pin!(decoding);
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        decoding.as_mut().poll(&mut context)
    };
    match polled {
        std::task::Poll::Ready(Ok(cut)) => Ok((output, cut)),
        std::task::Poll::Ready(Err(())) | std::task::Poll::Pending => Err(()),
    }
}

/// Decide one hop of a placeholder fetch; every hop, redirects included, is
/// evaluated as a new request.
///
/// Returns the URL to send when a rule substitutes a key into this hop, or
/// `None` to send `current` itself, whose placeholders then go out literally.
/// Only `query` rules substitute; a rule with any other location leaves its
/// placeholder literal. Each scanned label gets one audit record for the hop,
/// and a hop whose records cannot be written is not sent.
fn secret_hop(
    current: &Url,
    hop: usize,
    secrets: &SecretFetch<'_>,
    held: &mut HeldKeys,
) -> Result<Option<Url>, UrlFetchError> {
    let logical = current.as_str();
    let path = current.path();
    let target = match current.query() {
        Some(query) => format!("{path}?{query}"),
        None => path.to_string(),
    };
    // Only an HTTPS hop can carry a key.
    let authority = (current.scheme() == "https")
        .then(|| current.host_str().zip(current.port_or_known_default()))
        .flatten()
        .and_then(|(host, port)| {
            secret_rules::target::canonical_host(host)
                .ok()
                .map(|host| ConnectAuthority { host, port })
        });
    let Some(authority) = authority else {
        emit_hop_audit(secrets, held, hop, None, |_| {
            (Decision::Literal, AuditReason::OutOfScope)
        })
        .map_err(|_| secret_fetch_refused("audit_unavailable", logical))?;
        return Ok(None);
    };
    let refused = |held: &HeldKeys, reason: &str| {
        let _ = emit_hop_audit(secrets, held, hop, Some(&authority), |_| {
            (Decision::Refused, AuditReason::Refused)
        });
        secret_fetch_refused(reason, logical)
    };

    let rules: Vec<LabelRule> = secrets
        .rules
        .iter()
        .filter(|rule| matches!(rule.location, Location::Query(_)))
        .cloned()
        .collect();
    let request = RequestTarget {
        host: authority.host.clone(),
        port: authority.port,
        scheme: "https",
        target: &target,
    };
    let matched: Vec<bool> = rules
        .iter()
        .map(|rule| {
            request
                .matches(&RuleTarget {
                    host: &rule.host,
                    port: rule.port,
                    path_prefix: &rule.path_prefix,
                })
                .is_ok()
        })
        .collect();
    // Plan with a stand-in key first: the rules decide where a key would go,
    // and refuse an ambiguous request, before the vault releases anything.
    let planning: Vec<(String, Vec<u8>)> = rules
        .iter()
        .map(|rule| (rule.label.clone(), SECRET_PLANNING_KEY.to_vec()))
        .collect();
    let view = RequestView::new(&target, Vec::new());
    let plan = evaluate_request_with_connect(&view, &authority, &rules, &planning);
    if let Some(refusal) = plan.refusal {
        return Err(refused(&*held, refusal.reason()));
    }

    // Material is asked only for a rule this hop actually substitutes. A
    // rule whose host and path match while its placeholder is absent, not the
    // whole value, or in another parameter needs no key, and asking anyway
    // would spend a single-use grant on a request that sends none.
    let mut materials = Vec::with_capacity(rules.len());
    for ((rule, matched), planned) in rules.iter().zip(&matched).zip(&plan.labels) {
        if !matched || !planned.is_substituted() {
            materials.push(None);
            continue;
        }
        let Some(scope) = secrets.authorization.as_ref() else {
            return Err(refused(&*held, "missing_material"));
        };
        let material = match secrets.vault.material(scope, &rule.label, &rule.grant_id) {
            Ok(material) => material,
            Err(error) => {
                let _ = emit_hop_audit(secrets, held, hop, Some(&authority), |_| {
                    (Decision::Refused, AuditReason::Refused)
                });
                return Err(
                    match crate::secret::outcome_for_material(&rule.label, &error) {
                        crate::secret::LabelOutcome::Refuse { code, message } => {
                            UrlFetchError::secret(code, message)
                        }
                        _ => secret_fetch_refused("missing_material", logical),
                    },
                );
            }
        };
        if check_admissible(material.expose(), &rule.label, &rule.location).is_err() {
            return Err(refused(&*held, "material_not_admissible"));
        }
        // The matcher is installed before any byte of this hop is sent.
        if held.hold(&rule.label, material.expose()).is_err() {
            return Err(refused(&*held, "replacement_discloses_key"));
        }
        materials.push(Some(material));
    }

    let query_start = path.len() + 1;
    let mut edits: Vec<(usize, usize, Zeroizing<Vec<u8>>)> = Vec::new();
    for ((rule, outcome), material) in rules.iter().zip(&plan.labels).zip(&materials) {
        if let LabelOutcome::Substituted {
            byte_start,
            byte_end,
            ..
        } = outcome
        {
            let (Some(material), true) = (material, *byte_start >= query_start) else {
                return Err(refused(&*held, "missing_material"));
            };
            let encoded = encode_for_location(material.expose(), &rule.label, &rule.location)
                .map_err(|_| refused(&*held, "material_not_admissible"))?;
            edits.push((
                byte_start - query_start,
                byte_end - query_start,
                Zeroizing::new(encoded),
            ));
        }
    }
    let decide = |label: &str| -> (Decision, AuditReason) {
        let outcome = rules
            .iter()
            .position(|rule| rule.label == label)
            .and_then(|index| plan.labels.get(index));
        match outcome {
            Some(LabelOutcome::Substituted { .. }) => (Decision::Substituted, AuditReason::Matched),
            Some(LabelOutcome::NotWholeValue) => (Decision::Literal, AuditReason::NotWholeValue),
            _ => (Decision::Literal, AuditReason::OutOfScope),
        }
    };
    if edits.is_empty() {
        emit_hop_audit(secrets, held, hop, Some(&authority), decide)
            .map_err(|_| secret_fetch_refused("audit_unavailable", logical))?;
        return Ok(None);
    }

    edits.sort_by_key(|(start, _, _)| *start);
    let query = current.query().unwrap_or_default().as_bytes();
    let mut substituted = Zeroizing::new(Vec::with_capacity(query.len() + 64));
    let mut cursor = 0;
    for (start, end, replacement) in &edits {
        substituted.extend_from_slice(&query[cursor..*start]);
        substituted.extend_from_slice(replacement);
        cursor = *end;
    }
    substituted.extend_from_slice(&query[cursor..]);
    let Ok(substituted) = std::str::from_utf8(&substituted) else {
        return Err(refused(&*held, "malformed_query"));
    };
    // The path sent with a key is the normalized path the rule matched,
    // never another spelling of it.
    let Ok(normalized) = normalize_request_target(path) else {
        return Err(refused(&*held, "malformed_path"));
    };
    let Ok(wire_path) = std::str::from_utf8(&normalized.wire) else {
        return Err(refused(&*held, "malformed_path"));
    };
    let mut outbound = current.clone();
    outbound.set_path(wire_path);
    outbound.set_query(Some(substituted));
    emit_hop_audit(secrets, held, hop, Some(&authority), decide)
        .map_err(|_| secret_fetch_refused("audit_unavailable", logical))?;
    Ok(Some(outbound))
}

/// One audit record per scanned label for one hop. A record is handed to the
/// vault only after the crate's writer checked it against every held key.
fn emit_hop_audit(
    secrets: &SecretFetch<'_>,
    held: &HeldKeys,
    hop: usize,
    authority: Option<&ConnectAuthority>,
    decide: impl Fn(&str) -> (Decision, AuditReason),
) -> Result<(), ()> {
    let origin = format!("{}#{hop}", secrets.call_id);
    let keys = held.keys();
    let mut failed = false;
    for label in &secrets.labels {
        let (decision, reason) = decide(label);
        let mut record = AuditRecordV1::new(
            crate::secret::now_ms(),
            label,
            decision,
            reason,
            &secrets.call_id,
            &origin,
        )
        .with_scope(
            secrets
                .authorization
                .as_ref()
                .map(crate::secret::SecretScope::to_scope_ref),
        );
        let rule = secrets.rules.iter().find(|rule| &rule.label == label);
        if let (Some(authority), Some(rule)) = (authority, rule) {
            record = record.with_rule(
                &authority.host,
                authority.port,
                &rule.path_prefix,
                location_kind_token(&rule.location),
            );
        }
        if record.to_json_line_with_keys(&keys).is_err() || secrets.vault.audit(&record).is_err() {
            failed = true;
        }
    }
    if failed {
        Err(())
    } else {
        Ok(())
    }
}

/// What a cleanup sweep learned from one metadata file, kept so the next sweep
/// can skip re-reading it. `len` and `modified` identify the file version the
/// fetch time was read from: the cache rewrites metadata only through a rename
/// of a freshly written file, which always produces a new modification time.
#[derive(Debug, Clone)]
struct CleanupMemoEntry {
    len: u64,
    modified: SystemTime,
    fetched_at: u64,
    extension: String,
}

/// Per cache directory, the metadata facts of the last sweep, keyed by file
/// name. Every configure runs the cleanup sweep, and almost every metadata
/// file is unchanged since the previous one, so an unchanged file costs one
/// stat instead of an open, a read and a JSON parse. A sweep replaces its
/// directory's map with only the files it saw and kept, so removed entries do
/// not accumulate.
fn cleanup_memo() -> &'static std::sync::Mutex<HashMap<PathBuf, HashMap<String, CleanupMemoEntry>>>
{
    static MEMO: std::sync::OnceLock<
        std::sync::Mutex<HashMap<PathBuf, HashMap<String, CleanupMemoEntry>>>,
    > = std::sync::OnceLock::new();
    MEMO.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

#[cfg(test)]
thread_local! {
    /// Metadata files the cleanup sweep opened on this thread, for tests that
    /// check unchanged files are not read again.
    static CLEANUP_META_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub fn cleanup_url_cache(storage_dir: &Path) -> Result<usize, UrlFetchError> {
    cleanup_url_cache_at(storage_dir, now_ms())
}

fn cleanup_url_cache_at(storage_dir: &Path, now: u64) -> Result<usize, UrlFetchError> {
    let dir = cache_dir(storage_dir);
    if !dir.exists() {
        return Ok(0);
    }

    let entries = fs::read_dir(&dir).map_err(|error| {
        UrlFetchError::new(format!(
            "URL cache cleanup failed reading {}: {error}",
            dir.display()
        ))
    })?;
    let mut removed = 0usize;
    // Taken out for the sweep and put back at the end, so the lock is never
    // held across file I/O. A concurrent sweep of the same directory finds no
    // memo and reads every file, which is only slower.
    let previous = cleanup_memo()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&dir)
        .unwrap_or_default();
    let mut kept = HashMap::new();

    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !name.ends_with(".meta.json") {
            continue;
        }

        let version = entry
            .metadata()
            .ok()
            .and_then(|metadata| Some((metadata.len(), metadata.modified().ok()?)));
        let remembered = version.and_then(|(len, modified)| {
            previous
                .get(name)
                .filter(|known| known.len == len && known.modified == modified)
                .cloned()
        });
        let facts = match remembered {
            Some(known) => known,
            None => {
                #[cfg(test)]
                CLEANUP_META_READS.with(|reads| reads.set(reads.get() + 1));
                let meta = fs::read_to_string(&path)
                    .ok()
                    .and_then(|content| serde_json::from_str::<CacheMeta>(&content).ok());
                let Some(meta) = meta else {
                    if fs::remove_file(&path).is_ok() {
                        removed += 1;
                    }
                    continue;
                };
                let (len, modified) = version.unwrap_or((u64::MAX, UNIX_EPOCH));
                CleanupMemoEntry {
                    len,
                    modified,
                    fetched_at: meta.fetched_at,
                    extension: meta.extension,
                }
            }
        };

        if now.saturating_sub(facts.fetched_at) <= CACHE_TTL_MS {
            // A file whose version could not be read is not remembered, so the
            // next sweep reads it again.
            if version.is_some() {
                kept.insert(name.to_string(), facts);
            }
            continue;
        }

        let hash = name.trim_end_matches(".meta.json");
        let content = content_path(storage_dir, hash, &facts.extension);
        let _ = fs::remove_file(content);
        if fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }

    cleanup_memo()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(dir, kept);
    Ok(removed)
}

#[doc(hidden)]
pub fn cache_content_path_for_url(storage_dir: &Path, url: &str, extension: &str) -> PathBuf {
    content_path(storage_dir, &hash_url(url), extension)
}

#[doc(hidden)]
pub fn cache_meta_path_for_url(storage_dir: &Path, url: &str) -> PathBuf {
    meta_path(storage_dir, &hash_url(url))
}

/// Disclose a partial download separately from any later rendering cut. Keys
/// or headings after this prefix have not been inspected.
pub fn download_notice(path: &Path) -> Option<String> {
    let meta_path = path.with_extension("meta.json");
    let meta: CacheMeta = serde_json::from_slice(&fs::read(meta_path).ok()?).ok()?;
    if !meta.download_truncated {
        return None;
    }
    let total = meta
        .total_bytes
        .map(|n| n.to_string())
        .unwrap_or_else(|| format!("≥{}", meta.downloaded_bytes + 1));
    Some(format!(
        "Download truncated at {} of {total} bytes; narrow with a smaller URL or server-side filters (only the downloaded prefix was inspected)",
        meta.downloaded_bytes
    ))
}

#[doc(hidden)]
pub fn is_private_ip_for_test(ip: IpAddr) -> bool {
    is_private_ip(ip)
}

fn cache_dir(storage_dir: &Path) -> PathBuf {
    storage_dir.join("url_cache")
}

fn hash_url(url: &str) -> String {
    let digest = Sha256::digest(url.as_bytes());
    format!("{digest:x}").chars().take(16).collect()
}

/// Rewrite well-known web-view links before fetching because those pages serve
/// an HTML application shell instead of the file contents agents need to read.
fn rewrite_url_for_fetch(url: &Url) -> Url {
    let original = url.clone();
    let mut rewritten = url.clone();
    rewritten.set_fragment(None);

    let Some(host) = url.host_str() else {
        return rewritten;
    };
    if !matches!(url.scheme(), "http" | "https") {
        return rewritten;
    }

    let segments: Vec<&str> = url
        .path_segments()
        .map(|segments| segments.collect())
        .unwrap_or_default();
    let (raw_host, raw_path) = if matches!(host, "github.com" | "www.github.com") {
        if segments.len() >= 5 && matches!(segments[2], "blob" | "raw") {
            (
                "raw.githubusercontent.com",
                format!(
                    "/{}/{}/{}/{}",
                    segments[0],
                    segments[1],
                    segments[3],
                    segments[4..].join("/")
                ),
            )
        } else {
            return rewritten;
        }
    } else if host == "gitlab.com"
        && segments.len() >= 6
        && segments[2] == "-"
        && segments[3] == "blob"
    {
        (
            "gitlab.com",
            format!(
                "/{}/{}/-/raw/{}/{}",
                segments[0],
                segments[1],
                segments[4],
                segments[5..].join("/")
            ),
        )
    } else {
        return rewritten;
    };

    rewritten
        .set_host(Some(raw_host))
        .expect("raw content host should be a valid URL host");
    rewritten.set_path(&raw_path);
    log::debug!(
        "url_fetch: rewriting web-view URL to raw content: {} -> {}",
        original,
        rewritten
    );
    rewritten
}

fn meta_path(storage_dir: &Path, hash: &str) -> PathBuf {
    cache_dir(storage_dir).join(format!("{hash}.meta.json"))
}

fn content_path(storage_dir: &Path, hash: &str, extension: &str) -> PathBuf {
    cache_dir(storage_dir).join(format!("{hash}{extension}"))
}

fn fresh_cached_path(
    storage_dir: &Path,
    hash: &str,
    meta_file: &Path,
    url: &Url,
) -> Result<Option<PathBuf>, UrlFetchError> {
    #[cfg(test)]
    stall_cache_read_if_requested(storage_dir);
    if !meta_file.exists() {
        return Ok(None);
    }

    let meta = match fs::read_to_string(meta_file)
        .ok()
        .and_then(|content| serde_json::from_str::<CacheMeta>(&content).ok())
    {
        Some(meta) => meta,
        None => return Ok(None),
    };
    let age = now_ms().saturating_sub(meta.fetched_at);
    if meta.extension == ".html" {
        return Ok(None);
    }

    let content_type = meta.content_type.as_str();
    let current = resolve_fetch_extension(url, content_type);
    // An HTML page is stored converted to Markdown, so a URL whose path ends
    // in .html or .htm finds its cached copy under .md. Comparing against
    // .html would miss the cache on every call and fetch, convert and
    // re-parse the whole page again.
    let expected_ext = current.map(|(ext, _)| if ext == ".html" { ".md" } else { ext });
    if expected_ext != Some(meta.extension.as_str()) {
        return Ok(None);
    }

    let cached = content_path(storage_dir, hash, &meta.extension);
    // Old cache entries predate the iterator ceiling and may contain an
    // unbounded body. Refetch those rather than reintroducing the missed cap.
    let legacy_oversized = meta.downloaded_bytes == 0
        && fs::metadata(&cached).is_ok_and(|m| m.len() > MAX_RESPONSE_BYTES);
    if age < CACHE_TTL_MS && cached.exists() && !legacy_oversized {
        return Ok(Some(cached));
    }
    Ok(None)
}

fn fetch_with_redirects(
    start_url: &Url,
    original_url: &str,
    options: &UrlFetchOptions,
    deadline: &FetchDeadline,
    approved: Arc<ApprovedHosts>,
) -> Result<HttpResponse, UrlFetchError> {
    let progress = Arc::new(ConnectProgress::default());
    let client = build_client(options, Arc::clone(&approved), Arc::clone(&progress))?;
    let mut current_url = start_url.clone();

    for redirect_count in 0..=MAX_REDIRECTS {
        validate_public_url(&current_url, options, deadline, &approved)?;
        let response = send_with_transient_retries(&client, &current_url, deadline, &progress)?;

        if !response.status().is_redirection() {
            return Ok(response);
        }
        if redirect_count == MAX_REDIRECTS {
            return Err(UrlFetchError::new(format!(
                "Too many redirects fetching {original_url}"
            )));
        }

        let location = response
            .headers()
            .get(LOCATION)
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| {
                UrlFetchError::new(format!(
                    "Redirect from {} missing Location header",
                    current_url.as_str()
                ))
            })?;
        current_url = current_url.join(location).map_err(|error| {
            UrlFetchError::new(format!(
                "Invalid redirect Location '{location}' from {}: {error}",
                current_url.as_str()
            ))
        })?;
    }

    Err(UrlFetchError::new(format!(
        "Too many redirects fetching {original_url}"
    )))
}

/// Issue a single GET with the configured User-Agent + Accept headers and
/// transparently retry only on transient connect/transport failures.
///
/// Returns the response (including 4xx/5xx — caller decides how to treat
/// those). On a non-transient reqwest error (e.g. an HTTP-shaped reply that
/// reqwest still surfaces as Err, or a TLS handshake fault that doesn't read
/// as `is_connect`), the original error is returned immediately so the user
/// sees the real failure without an artificial 800ms-plus delay.
///
/// Every attempt is limited to the time left before `deadline`, and a retry
/// runs only when its backoff plus [`MIN_USEFUL_ATTEMPT`] still fit, so
/// retries can never stretch a fetch past its deadline.
fn send_with_transient_retries(
    client: &Client,
    target: &Url,
    deadline: &FetchDeadline,
    progress: &ConnectProgress,
) -> Result<HttpResponse, UrlFetchError> {
    send_request(client, target, None, deadline, progress)
}

/// [`send_with_transient_retries`] for one request. With `secret` set, the
/// request belongs to a placeholder fetch: every message names the given
/// placeholder URL instead of `target`, a failure is reported through
/// [`send_failure`] with the held keys, and the request asks for an unencoded
/// response so the body can be scrubbed byte for byte.
fn send_request(
    client: &Client,
    target: &Url,
    secret: Option<(&str, &HeldKeys)>,
    deadline: &FetchDeadline,
    progress: &ConnectProgress,
) -> Result<HttpResponse, UrlFetchError> {
    let named = secret.map_or(target.as_str(), |(shown, _)| shown);
    let held = secret.map(|(_, held)| held);
    let mut attempt = 0;
    loop {
        let remaining = deadline.remaining();
        if remaining.is_zero() {
            return Err(deadline.timeout_error(FetchPhase::Connect, named));
        }
        // A per-request timeout replaces reqwest's default 30 s, which would
        // otherwise let one silent server hold each attempt for 30 s.
        progress.begin_attempt();
        let mut request = client
            .get(target.clone())
            .header(USER_AGENT, USER_AGENT_VALUE)
            .header(ACCEPT, ACCEPT_HEADER)
            .timeout(remaining.min(HEADERS_TIMEOUT));
        if held.is_some() {
            request = request.header(ACCEPT_ENCODING, "identity");
        }
        let error = match request.send() {
            Ok(response) => return Ok(response),
            Err(error) => error,
        };
        let phase = error
            .is_timeout()
            .then(|| timed_out_phase(&error, progress));
        // A server that accepted the request and stayed silent until the
        // headers limit is not a passing connection fault; retrying it would
        // only spend the rest of the deadline the same way.
        let retryable = is_transient_reqwest_error(&error) && phase != Some(FetchPhase::Headers);
        let backoff = TRANSIENT_RETRY_BACKOFFS_MS
            .get(attempt)
            .map(|ms| Duration::from_millis(*ms))
            .filter(|backoff| retryable && *backoff + MIN_USEFUL_ATTEMPT <= deadline.remaining());
        if let Some(backoff) = backoff {
            thread::sleep(backoff);
            attempt += 1;
            continue;
        }
        return Err(send_failure(error, phase, deadline, named, held));
    }
}

/// The error for a request that failed for good, naming `named`. For a
/// placeholder fetch (`held` set) reqwest's error first loses its URL, which
/// carried the key, and the detail left is scrubbed of every held key (or
/// dropped if it still spells one).
fn send_failure(
    error: reqwest::Error,
    phase: Option<FetchPhase>,
    deadline: &FetchDeadline,
    named: &str,
    held: Option<&HeldKeys>,
) -> UrlFetchError {
    let error = if held.is_some() {
        error.without_url()
    } else {
        error
    };
    let detail = reqwest_error_detail(&error);
    let detail = match held {
        Some(held) => held.sanitize_detail(&detail),
        None => detail,
    };
    if let Some(phase) = phase {
        return request_timeout_error(&detail, phase, deadline, named);
    }
    if detail.is_empty() {
        return UrlFetchError::new(format!("Failed to fetch {named}"));
    }
    UrlFetchError::new(format!("Failed to fetch {named}: {detail}"))
}

/// Name the phase a timed-out request was in: a name lookup still running in
/// the resolver, then connection setup still running (or reported by reqwest
/// as the cause), and otherwise the wait for the response headers.
fn timed_out_phase(error: &reqwest::Error, progress: &ConnectProgress) -> FetchPhase {
    if progress.lookup_pending() {
        FetchPhase::Dns
    } else if progress.connect_pending() || error.is_connect() {
        FetchPhase::Connect
    } else {
        FetchPhase::Headers
    }
}

fn request_timeout_error(
    detail: &str,
    phase: FetchPhase,
    deadline: &FetchDeadline,
    target: &str,
) -> UrlFetchError {
    if deadline.expired() {
        return deadline.timeout_error(phase, target);
    }
    let limit = match phase {
        FetchPhase::Dns | FetchPhase::Connect => CONNECT_TIMEOUT,
        FetchPhase::Headers | FetchPhase::Body => HEADERS_TIMEOUT,
    };
    UrlFetchError::timeout(
        phase,
        format!(
            "URL fetch timed out in the {} phase after the {}ms limit fetching {}: {}",
            phase.as_str(),
            limit.as_millis(),
            target,
            detail
        ),
    )
}

/// Classify a reqwest error as transient (worth a quick retry) vs terminal.
///
/// Transient: TCP connect failures, request-build/send TCP-level failures
/// that don't carry status, and timeouts. These typically clear on a single
/// retry — agents shouldn't have to ask twice for a momentary blip.
///
/// Terminal: anything where reqwest got far enough to decode an HTTP-shaped
/// reply (`is_status()`, `is_body()`, `is_decode()`). Retrying those would
/// just hammer a server that already answered.
fn is_transient_reqwest_error(error: &reqwest::Error) -> bool {
    error.is_connect() || error.is_timeout() || error.is_request()
}

fn build_client(
    options: &UrlFetchOptions,
    approved: Arc<ApprovedHosts>,
    progress: Arc<ConnectProgress>,
) -> Result<Client, UrlFetchError> {
    let resolver = GuardedResolver {
        approved,
        connect_overrides: options.connect_overrides.clone(),
        lookup_override: options.lookup_override.clone(),
        progress: Arc::clone(&progress),
    };
    let builder = Client::builder()
        .redirect(Policy::none())
        .connect_timeout(CONNECT_TIMEOUT)
        .connector_layer(TrackConnect {
            progress,
            delay: options.connect_delay,
        })
        .dns_resolver(Arc::new(resolver));

    let tls_config = match &options.tls_config {
        Some(config) => config.clone(),
        None => crate::platform_tls::client_config().map_err(|error| {
            UrlFetchError::new(format!("Failed to configure URL fetch TLS: {error}"))
        })?,
    };

    builder
        .use_preconfigured_tls(tls_config)
        .build()
        .map_err(|error| UrlFetchError::new(format!("Failed to build URL fetch client: {error}")))
}

/// Host names the SSRF check resolved and approved during one fetch, with
/// the addresses it approved. The resolver hands reqwest exactly these, so
/// the connection cannot reach an address that a second lookup returned (a
/// DNS-rebinding answer), and a redirect back to the same host needs no new
/// lookup.
#[derive(Default)]
struct ApprovedHosts(std::sync::Mutex<HashMap<String, Vec<IpAddr>>>);

impl ApprovedHosts {
    fn get(&self, host: &str) -> Option<Vec<IpAddr>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(host)
            .cloned()
    }

    fn insert(&self, host: &str, addresses: Vec<IpAddr>) {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(host.to_string(), addresses);
    }
}

/// Connection-setup progress of the current request attempt, read when the
/// request times out to name the phase it was in. A step that a timeout
/// abandoned never reports its end, so each attempt starts from a clean
/// state and a step reports its end only to the attempt that started it.
#[derive(Default)]
struct ConnectProgress(std::sync::Mutex<AttemptProgress>);

#[derive(Default)]
struct AttemptProgress {
    attempt: u64,
    lookup_pending: bool,
    connect_pending: bool,
}

impl ConnectProgress {
    fn state(&self) -> std::sync::MutexGuard<'_, AttemptProgress> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn begin_attempt(&self) {
        let mut state = self.state();
        state.attempt += 1;
        state.lookup_pending = false;
        state.connect_pending = false;
    }

    /// Returns the attempt the connection belongs to.
    fn connect_started(&self) -> u64 {
        let mut state = self.state();
        state.connect_pending = true;
        state.attempt
    }

    fn connected(&self, attempt: u64) {
        let mut state = self.state();
        if state.attempt == attempt {
            state.connect_pending = false;
        }
    }

    fn connect_pending(&self) -> bool {
        self.state().connect_pending
    }

    /// Returns the attempt the lookup belongs to.
    fn lookup_started(&self) -> u64 {
        let mut state = self.state();
        state.lookup_pending = true;
        state.attempt
    }

    fn lookup_answered(&self, attempt: u64) {
        let mut state = self.state();
        if state.attempt == attempt {
            state.lookup_pending = false;
        }
    }

    fn lookup_pending(&self) -> bool {
        self.state().lookup_pending
    }
}

/// Connector layer that records when a request starts and finishes setting
/// up its connection, so a timeout can tell a stalled connect from a server
/// that accepted the connection and never answered.
#[derive(Clone)]
struct TrackConnect {
    progress: Arc<ConnectProgress>,
    delay: Option<Duration>,
}

impl<S> tower_layer::Layer<S> for TrackConnect {
    type Service = TrackedConnector<S>;

    fn layer(&self, inner: S) -> Self::Service {
        TrackedConnector {
            inner,
            progress: Arc::clone(&self.progress),
            delay: self.delay,
        }
    }
}

#[derive(Clone)]
struct TrackedConnector<S> {
    inner: S,
    progress: Arc<ConnectProgress>,
    delay: Option<Duration>,
}

impl<S, Target> tower_service::Service<Target> for TrackedConnector<S>
where
    S: tower_service::Service<Target>,
    S::Future: Send + 'static,
    S::Response: Send + 'static,
    S::Error: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future =
        std::pin::Pin<Box<dyn std::future::Future<Output = Result<S::Response, S::Error>> + Send>>;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, target: Target) -> Self::Future {
        let attempt = self.progress.connect_started();
        let progress = Arc::clone(&self.progress);
        let delay = self.delay;
        let connecting = self.inner.call(target);
        Box::pin(async move {
            if let Some(delay) = delay {
                tokio::time::sleep(delay).await;
            }
            let connection = connecting.await;
            if connection.is_ok() {
                progress.connected(attempt);
            }
            connection
        })
    }
}

/// The resolver reqwest uses for every connection of a fetch.
///
/// A host the SSRF check approved gets exactly the approved addresses. Any
/// other host is looked up here without the SSRF check: that is a proxy host,
/// or any host when private addresses are allowed. The lookup runs on its own
/// thread so the request's timeout can abandon it: the request stops waiting
/// at the fetch deadline, although the system resolver's thread may run on
/// until its lookup returns.
struct GuardedResolver {
    approved: Arc<ApprovedHosts>,
    connect_overrides: Vec<(String, SocketAddr)>,
    lookup_override: Option<HostLookup>,
    progress: Arc<ConnectProgress>,
}

impl reqwest::dns::Resolve for GuardedResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        // Port 0 tells reqwest to use the URL's port.
        let ready = |addresses: Vec<SocketAddr>| -> reqwest::dns::Resolving {
            Box::pin(std::future::ready(Ok(
                Box::new(addresses.into_iter()) as reqwest::dns::Addrs
            )))
        };
        if let Some((_, address)) = self
            .connect_overrides
            .iter()
            .find(|(override_host, _)| *override_host == host)
        {
            return ready(vec![*address]);
        }
        if let Some(addresses) = self.approved.get(&host) {
            return ready(
                addresses
                    .into_iter()
                    .map(|ip| SocketAddr::new(ip, 0))
                    .collect(),
            );
        }
        let progress = Arc::clone(&self.progress);
        let attempt = progress.lookup_started();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let lookup = self.lookup_override.clone();
        thread::spawn(move || {
            let _ = sender.send(lookup_host(&host, lookup.as_ref()));
        });
        Box::pin(async move {
            let answer: Result<io::Result<Vec<IpAddr>>, _> = receiver.await;
            progress.lookup_answered(attempt);
            let addresses = answer
                .unwrap_or_else(|_| Err(io::Error::other("host lookup stopped before answering")))
                .map_err(|error| Box::new(error) as Box<dyn Error + Send + Sync>)?;
            Ok(
                Box::new(addresses.into_iter().map(|ip| SocketAddr::new(ip, 0)))
                    as reqwest::dns::Addrs,
            )
        })
    }
}

fn lookup_host(host: &str, lookup_override: Option<&HostLookup>) -> io::Result<Vec<IpAddr>> {
    if let Some(lookup) = lookup_override {
        return lookup(host);
    }
    Ok((host, 0).to_socket_addrs()?.map(|addr| addr.ip()).collect())
}

/// Refuse a URL that is not http(s) or that points at a private address.
/// A host name is resolved under the fetch deadline and its approved
/// addresses are recorded in `approved` for the resolver.
fn validate_public_url(
    url: &Url,
    options: &UrlFetchOptions,
    deadline: &FetchDeadline,
    approved: &ApprovedHosts,
) -> Result<(), UrlFetchError> {
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(UrlFetchError::new(format!(
            "Only http:// and https:// URLs are supported, got: {}:",
            url.scheme()
        )));
    }
    if options.allow_private {
        return Ok(());
    }

    let host = url
        .host_str()
        .ok_or_else(|| UrlFetchError::new(format!("URL missing host: {url}")))?;
    let host_for_parse = host
        .trim_matches(['[', ']'])
        .split('%')
        .next()
        .unwrap_or(host);

    if let Ok(ip) = host_for_parse.parse::<IpAddr>() {
        reject_private_ip(host, ip)?;
        return Ok(());
    }
    if host_for_parse.contains(':') {
        return Err(UrlFetchError::new(format!(
            "Blocked private URL host {host} ({host_for_parse})"
        )));
    }
    if approved.get(host_for_parse).is_some() {
        return Ok(());
    }

    let addresses = resolve_host_ips(host_for_parse, url.as_str(), options, deadline)?;
    if addresses.is_empty() {
        return Err(UrlFetchError::new(format!(
            "Failed to resolve URL host {host}"
        )));
    }
    for ip in &addresses {
        reject_private_ip(host, *ip)?;
    }
    approved.insert(host_for_parse, addresses);
    Ok(())
}

/// Resolve `host` on a helper thread and wait for it no longer than the
/// fetch deadline; the system resolver itself has no timeout.
fn resolve_host_ips(
    host: &str,
    url: &str,
    options: &UrlFetchOptions,
    deadline: &FetchDeadline,
) -> Result<Vec<IpAddr>, UrlFetchError> {
    if let Some((_, ips)) = options
        .public_host_overrides
        .iter()
        .find(|(override_host, _)| override_host == host)
    {
        return Ok(ips.clone());
    }

    let remaining = deadline.remaining();
    if remaining.is_zero() {
        return Err(deadline.timeout_error(FetchPhase::Dns, url));
    }
    let (sender, receiver) = mpsc::channel();
    let owned_host = host.to_string();
    let lookup = options.lookup_override.clone();
    thread::spawn(move || {
        let _ = sender.send(lookup_host(&owned_host, lookup.as_ref()));
    });
    match receiver.recv_timeout(remaining) {
        Ok(Ok(addresses)) => Ok(addresses),
        Ok(Err(error)) => Err(UrlFetchError::new(format!(
            "Failed to resolve URL host {host}: {error}"
        ))),
        Err(mpsc::RecvTimeoutError::Timeout) => Err(deadline.timeout_error(FetchPhase::Dns, url)),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(UrlFetchError::new(format!(
            "Failed to resolve URL host {host}: lookup stopped before answering"
        ))),
    }
}

fn reject_private_ip(host: &str, ip: IpAddr) -> Result<(), UrlFetchError> {
    if is_private_ip(ip) {
        return Err(UrlFetchError::new(format!(
            "Blocked private URL host {host} ({ip})"
        )));
    }
    Ok(())
}

/// True for any private/link-local/CGNAT/benchmark/multicast/reserved/loopback
/// address (the full set this module refuses to fetch). Exposed so the semantic
/// embedding SSRF guard shares one authoritative range list instead of keeping a
/// drifting copy. Note: this INCLUDES loopback — callers that intentionally
/// allow loopback (e.g. a local Ollama endpoint) must exclude it themselves.
pub fn is_private_or_reserved_ip(ip: IpAddr) -> bool {
    is_private_ip(ip)
}

fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ipv4) => is_private_ipv4(ipv4),
        IpAddr::V6(ipv6) => is_private_ipv6(ipv6),
    }
}

fn is_private_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, _, _] = ip.octets();
    a == 0
        || a == 10
        || a == 127
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 168)
        || (a == 169 && b == 254)
        // RFC 6598 Shared Address Space (CGNAT): 100.64.0.0/10. Not globally
        // routable; used for provider/VPC-internal endpoints — must not be
        // reachable via SSRF.
        || (a == 100 && (64..=127).contains(&b))
        // RFC 2544 benchmark subnet: 198.18.0.0/15. Reserved, non-routable.
        || (a == 198 && (18..=19).contains(&b))
        || a >= 224
}

fn is_private_ipv6(ip: Ipv6Addr) -> bool {
    let segments = ip.segments();
    let top_six_zero = segments[..6].iter().all(|segment| *segment == 0);
    let is_mapped = segments[..5].iter().all(|segment| *segment == 0) && segments[5] == 0xffff;
    if is_mapped || top_six_zero {
        let embedded = Ipv4Addr::new(
            (segments[6] >> 8) as u8,
            (segments[6] & 0xff) as u8,
            (segments[7] >> 8) as u8,
            (segments[7] & 0xff) as u8,
        );
        return is_private_ipv4(embedded);
    }

    let first = segments[0];
    // fe80::/10 link-local, fec0::/10 deprecated site-local (still routed
    // inside some networks), fc00::/7 unique local, ff00::/8 multicast.
    (0xfe80..=0xfeff).contains(&first) || (0xfc00..=0xfdff).contains(&first) || first >= 0xff00
}

const BINARY_SNIFF_PREFIX: usize = 8 * 1024;

fn body_contains_nul_in_prefix(body: &[u8]) -> bool {
    let end = body.len().min(BINARY_SNIFF_PREFIX);
    body[..end].contains(&0)
}

/// Resolve cache extension: URL path (parsed, percent-decoded) first for source
/// languages AFT parses; otherwise content-type mapping.
fn resolve_fetch_extension(url: &Url, content_type: &str) -> Option<(&'static str, bool)> {
    if let Some(ext) = extension_from_url_path(url) {
        return Some((ext, true));
    }
    resolve_extension_from_content_type(content_type).map(|ext| (ext, false))
}

fn extension_from_url_path(url: &Url) -> Option<&'static str> {
    let path = url.path();
    if path.is_empty() || path == "/" {
        return None;
    }
    let segment = path.rsplit('/').next().unwrap_or(path);
    let file_name = percent_decode_path_segment(segment);
    let dot = file_name.rfind('.')?;
    let ext = &file_name[dot + 1..];
    if ext.is_empty() {
        return None;
    }
    let probe = Path::new("file").with_extension(ext);
    if detect_language(&probe).is_some() {
        static_extension_for_lang_ext(ext)
    } else {
        None
    }
}

fn percent_decode_path_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    let bytes = segment.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h1), Some(h2)) = (from_hex(bytes[i + 1]), from_hex(bytes[i + 2])) {
                out.push(char::from(h1 << 4 | h2));
                i += 3;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn from_hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Dotted cache extension for a path extension that `detect_language` accepts.
fn static_extension_for_lang_ext(ext: &str) -> Option<&'static str> {
    match ext.to_ascii_lowercase().as_str() {
        "ts" | "mts" | "cts" => Some(".ts"),
        "tsx" => Some(".tsx"),
        "js" => Some(".js"),
        "jsx" => Some(".jsx"),
        "mjs" => Some(".mjs"),
        "cjs" => Some(".cjs"),
        "py" | "pyi" => Some(".py"),
        "rs" => Some(".rs"),
        "go" => Some(".go"),
        "c" | "h" => Some(".c"),
        "cc" | "cpp" | "cxx" | "hpp" | "hh" => Some(".cpp"),
        "zig" => Some(".zig"),
        "cs" => Some(".cs"),
        "sh" | "bash" | "zsh" => Some(".sh"),
        "html" | "htm" => Some(".html"),
        "md" | "markdown" | "mdx" => Some(".md"),
        "sol" => Some(".sol"),
        "scss" => Some(".scss"),
        "vue" => Some(".vue"),
        "json" | "jsonc" => Some(".json"),
        "scala" | "sc" => Some(".scala"),
        "java" => Some(".java"),
        "rb" => Some(".rb"),
        "kt" | "kts" => Some(".kt"),
        "swift" => Some(".swift"),
        "inc" | "php" => Some(".php"),
        "lua" => Some(".lua"),
        "pl" | "pm" | "t" => Some(".pl"),
        "yaml" | "yml" => Some(".yaml"),
        _ => None,
    }
}

fn resolve_extension_from_content_type(content_type: &str) -> Option<&'static str> {
    let lower = content_type.to_ascii_lowercase();
    let media_type = lower
        .split(';')
        .next()
        .unwrap_or("")
        .split(',')
        .next()
        .unwrap_or("")
        .trim();

    match media_type {
        "text/html"
        | "application/xhtml+xml"
        | "application/vnd.github.html"
        | "application/vnd.github+html" => Some(".html"),
        "text/markdown"
        | "text/x-markdown"
        | "application/markdown"
        | "application/vnd.github.raw"
        | "application/vnd.github+raw"
        | "application/vnd.github.v3.raw"
        | "text/plain" => Some(".md"),
        "application/json" | "application/ld+json" => Some(".json"),
        other if other.ends_with("+json") => Some(".json"),
        "text/javascript" | "application/javascript" | "application/ecmascript" => Some(".js"),
        "text/typescript" | "application/typescript" => Some(".ts"),
        _ => None,
    }
}

fn convert_html_body_to_markdown(body: &[u8], url: &str) -> Result<Vec<u8>, UrlFetchError> {
    let html = String::from_utf8_lossy(body);
    let mut markdown = html_to_markdown_converter()
        .convert(&html)
        .map_err(|error| {
            UrlFetchError::new(format!(
                "Failed to convert HTML from {url} to Markdown: {error}"
            ))
        })?;
    if !markdown.ends_with('\n') {
        markdown.push('\n');
    }
    Ok(markdown.into_bytes())
}

/// Page chrome dropped from the converted Markdown. The `<pre>` text walker
/// skips the same tags so a copy button inside a code block stays out of it.
const SKIPPED_HTML_TAGS: [&str; 8] = [
    "head", "script", "style", "nav", "footer", "aside", "noscript", "button",
];

fn html_to_markdown_converter() -> HtmlToMarkdown {
    HtmlToMarkdown::builder()
        .skip_tags(SKIPPED_HTML_TAGS.to_vec())
        .add_handler(
            vec!["pre"],
            |handlers: &dyn Handlers, element: Element| -> Option<HandlerResult> {
                if node_has_descendant_tag(element.node, "code") {
                    Some(fenced_code_block(&element).into())
                } else {
                    handlers.fallback(element)
                }
            },
        )
        .add_handler(
            vec!["a"],
            |handlers: &dyn Handlers, element: Element| -> Option<HandlerResult> {
                if is_permalink_anchor(&element)
                    || handlers.walk_children(element.node).content.trim() == "§"
                {
                    None
                } else if element_attr_value(&element, "href")
                    .is_some_and(|href| href.trim() == "#")
                {
                    Some(handlers.walk_children(element.node).content.into())
                } else {
                    handlers.fallback(element)
                }
            },
        )
        .add_handler(
            vec!["header"],
            |handlers: &dyn Handlers, element: Element| -> Option<HandlerResult> {
                if should_skip_header(&element) {
                    None
                } else {
                    handlers.fallback(element)
                }
            },
        )
        .add_handler(
            vec!["span"],
            |handlers: &dyn Handlers, element: Element| -> Option<HandlerResult> {
                if element_has_class_token(&element, "token-line") {
                    let mut content = handlers.walk_children(element.node).content;
                    content.push('\n');
                    Some(content.into())
                } else {
                    handlers.fallback(element)
                }
            },
        )
        .build()
}

/// Render a `<pre>` that holds code as one fenced block built from the raw
/// text of the whole `<pre>`, not just its `<code>` child.
///
/// htmd fences only the `<code>` element and renders whatever follows it in
/// the `<pre>` as ordinary text. Real pages close `</code>` early: the Chrome
/// DevTools docs end `<code>` in the middle of a JSON string, and the HTML
/// parser then leaves the rest of the block as `<pre>` children after the
/// `<code>`. A browser still shows that as one block, so the Markdown must too.
/// Walking the text ourselves also keeps htmd's text handling (escaping,
/// collapsing blank lines) away from code that has to be copied verbatim.
fn fenced_code_block(element: &Element<'_>) -> String {
    let mut text = PreformattedText::default();
    text.collect(element.node);
    // A break still pending after the last line ends the block, so it is
    // dropped rather than written.
    let mut code = text.output;
    // The line break before `</code>` or `</pre>` ends the last line; it is
    // not an extra blank line inside the block.
    if code.ends_with('\n') {
        code.pop();
    }
    let fence = code_fence_for(&code);
    let language = find_descendant_element(element.node, "code")
        .and_then(|code| node_class_attr(&code))
        .as_deref()
        .and_then(language_from_class)
        .or_else(|| element_attr_value(element, "class").and_then(language_from_class));

    let mut block = String::from("\n\n");
    block.push_str(&fence);
    if let Some(language) = language {
        block.push_str(&language);
    }
    block.push('\n');
    block.push_str(&code);
    block.push('\n');
    block.push_str(&fence);
    block.push_str("\n\n");
    block
}

/// How an element inside a `<pre>` ends the text line it holds.
///
/// Many highlighters emit one element per source line with no newline text
/// between them: Prism `span.token-line` (Docusaurus), Shiki `span.line`,
/// `<li>` per line, table rows (highlight.js line-numbers plugin), or plain
/// per-line `<div>`s. A browser still shows separate lines, so the copied
/// text must break after each one.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PreLineBreak {
    /// Inline element: adds no break.
    None,
    /// One source line, so it always ends a line, even when empty (an empty
    /// row or `span.line` is a blank line of code).
    Line,
    /// Generic block (`<div>`, `<p>`): ends a line only when it holds text,
    /// so an empty line-number `<div>` beside the code does not add a blank
    /// line.
    Block,
}

fn pre_line_break(tag: &str, class: Option<&str>) -> PreLineBreak {
    let has_class =
        |token: &str| class.is_some_and(|value| value.split_ascii_whitespace().any(|c| c == token));
    match tag {
        "tr" | "li" => PreLineBreak::Line,
        "span" if has_class("token-line") || has_class("line") => PreLineBreak::Line,
        "div" | "p" => PreLineBreak::Block,
        _ => PreLineBreak::None,
    }
}

/// The raw text of a `<pre>`, collected exactly as written: no escaping and
/// no whitespace changes.
///
/// A per-line element (see [`PreLineBreak`]) ends its line with a *pending*
/// break rather than a written one. Some highlighters already put a newline
/// between their line elements (Shiki's default output does) and some do
/// not; the pending break is written only when more of the block follows
/// and is absorbed when that next text already starts with a newline, so
/// both layouts give one line per element.
#[derive(Default)]
struct PreformattedText {
    output: String,
    pending_break: bool,
}

impl PreformattedText {
    fn flush_pending_break(&mut self) {
        if self.pending_break {
            self.output.push('\n');
            self.pending_break = false;
        }
    }

    fn push_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        if self.pending_break && text.starts_with('\n') {
            self.pending_break = false;
        }
        self.flush_pending_break();
        self.output.push_str(text);
    }

    fn collect(&mut self, node: &Node) {
        for child in node.children.borrow().iter() {
            match &child.data {
                NodeData::Text { contents } => self.push_text(&contents.borrow()),
                NodeData::Element { name, attrs, .. } => {
                    let tag = name.local.as_ref();
                    if SKIPPED_HTML_TAGS.contains(&tag) {
                        continue;
                    }
                    if tag == "br" {
                        // A `<br>` right after a finished line starts an
                        // empty line, as it does in a browser.
                        self.flush_pending_break();
                        self.output.push('\n');
                        continue;
                    }
                    let line_break = {
                        let attrs = attrs.borrow();
                        let class = attrs
                            .iter()
                            .find(|attr| attr.name.local.as_ref() == "class")
                            .map(|attr| attr.value.as_ref());
                        pre_line_break(tag, class)
                    };
                    if line_break == PreLineBreak::None {
                        self.collect(child);
                        continue;
                    }
                    // A new line element starts on its own line, so the
                    // previous one's break is written before measuring this
                    // one's text.
                    self.flush_pending_break();
                    let line_start = self.output.len();
                    self.collect(child);
                    let line = &self.output[line_start..];
                    let line_ended = self.pending_break || line.ends_with('\n');
                    let ends_line = match line_break {
                        PreLineBreak::None => false,
                        PreLineBreak::Line => !line_ended,
                        PreLineBreak::Block => !line.is_empty() && !line_ended,
                    };
                    if ends_line {
                        self.pending_break = true;
                    }
                }
                _ => {}
            }
        }
    }
}

/// A backtick fence one longer than the longest backtick run in `code`
/// (at least three), so a run inside the code can never close the block.
fn code_fence_for(code: &str) -> String {
    let mut longest = 0;
    let mut current = 0;
    for ch in code.chars() {
        if ch == '`' {
            current += 1;
            longest = longest.max(current);
        } else {
            current = 0;
        }
    }
    "`".repeat((longest + 1).max(3))
}

/// The `xyz` of a `language-xyz` class token, the convention both Prism and
/// highlight.js use to name a code block's language.
fn language_from_class(class: &str) -> Option<String> {
    class
        .split_ascii_whitespace()
        .find_map(|token| token.strip_prefix("language-"))
        .filter(|language| !language.is_empty())
        .map(str::to_string)
}

fn node_has_descendant_tag(node: &Node, tag: &str) -> bool {
    find_descendant_element(node, tag).is_some()
}

/// The first element named `tag` below `node`, depth first.
fn find_descendant_element(node: &Node, tag: &str) -> Option<Rc<Node>> {
    for child in node.children.borrow().iter() {
        if let NodeData::Element { name, .. } = &child.data {
            if name.local.as_ref() == tag {
                return Some(Rc::clone(child));
            }
            if let Some(found) = find_descendant_element(child, tag) {
                return Some(found);
            }
        }
    }
    None
}

fn node_class_attr(node: &Node) -> Option<String> {
    match &node.data {
        NodeData::Element { attrs, .. } => attrs
            .borrow()
            .iter()
            .find(|attr| attr.name.local.as_ref() == "class")
            .map(|attr| attr.value.to_string()),
        _ => None,
    }
}

fn is_permalink_anchor(element: &Element<'_>) -> bool {
    element_has_class_token(element, "hash-link")
        || element_has_class_token(element, "doc-anchor")
        || element_attr_value(element, "aria-label")
            .is_some_and(|value| value.to_ascii_lowercase().starts_with("direct link to"))
}

fn should_skip_header(element: &Element<'_>) -> bool {
    element_has_class_token(element, "navbar")
        || element_has_class_token(element, "site-header")
        || element_has_class_token(element, "site-nav")
        || element_has_class_token(element, "topbar")
        || element_attr_value(element, "role")
            .is_some_and(|value| value.eq_ignore_ascii_case("banner"))
        || element_attr_value(element, "id").is_some_and(|value| {
            let value = value.to_ascii_lowercase();
            value.contains("navbar") || value.contains("site-header") || value.contains("site-nav")
        })
}

fn element_has_class_token(element: &Element<'_>, token: &str) -> bool {
    element_attr_value(element, "class")
        .is_some_and(|value| value.split_ascii_whitespace().any(|class| class == token))
}

fn element_attr_value<'a>(element: &'a Element<'_>, name: &str) -> Option<&'a str> {
    element
        .attrs
        .iter()
        .find(|attr| attr.name.local.as_ref() == name)
        .map(|attr| attr.value.as_ref())
}

enum BodyReadEvent {
    Chunk(Vec<u8>),
    Done,
    /// A read failed after waiting this long for data.
    Error(io::ErrorKind, String, Duration),
}

/// Read at most [`MAX_RESPONSE_BYTES`] (plus one lookahead byte) of the body.
/// Two limits stop a slow body: no data for [`BODY_CHUNK_TIMEOUT`], and the
/// fetch deadline, which also stops a server that sends a little data just
/// often enough to avoid the stall limit.
///
/// Whether a failed read was a timeout is decided mainly by time: reqwest
/// reports both its per-read limit and its whole-request limit as an
/// `io::Error` of kind `Other` (its blocking `Read` impl wraps the timeout
/// with `into_io`), and which of those timers or this function's own wait
/// fires first differs between platforms. So a failed read is a body-phase
/// timeout when the fetch deadline has passed, when the read had waited at
/// least [`BODY_CHUNK_TIMEOUT`] for data, or when its kind is `TimedOut` or
/// `WouldBlock`.
fn read_response_body(
    response: impl Read + Send + 'static,
    url: &str,
    deadline: &FetchDeadline,
) -> Result<(Vec<u8>, bool), UrlFetchError> {
    read_response_body_with(response, url, deadline, &|detail: &str| detail.to_string())
}

/// [`read_response_body`] with `detail` applied to the text of a failed read
/// before it goes into the error, so a placeholder fetch can scrub it.
fn read_response_body_with(
    response: impl Read + Send + 'static,
    url: &str,
    deadline: &FetchDeadline,
    detail: &dyn Fn(&str) -> String,
) -> Result<(Vec<u8>, bool), UrlFetchError> {
    // Bound the producer, not just its consumer: an unbounded channel can
    // queue the entire response before the caller notices the byte ceiling.
    let (tx, rx) = mpsc::sync_channel(1);
    thread::spawn(move || {
        // One lookahead byte distinguishes an exact-size body from a cut,
        // including chunked responses without Content-Length.
        let mut response = response.take(MAX_RESPONSE_BYTES + 1);
        let mut buffer = [0u8; 16 * 1024];
        loop {
            let read_started = Instant::now();
            match response.read(&mut buffer) {
                Ok(0) => {
                    let _ = tx.send(BodyReadEvent::Done);
                    break;
                }
                Ok(n) => {
                    if tx.send(BodyReadEvent::Chunk(buffer[..n].to_vec())).is_err() {
                        break;
                    }
                }
                Err(error) => {
                    let kind = error.kind();
                    let message = error.to_string();
                    let waited = read_started.elapsed();
                    let _ = tx.send(BodyReadEvent::Error(kind, message, waited));
                    break;
                }
            }
        }
    });

    let mut chunks = Vec::new();
    let mut total = 0u64;
    let slow_body_error = || {
        if deadline.expired() {
            deadline.timeout_error(FetchPhase::Body, url)
        } else {
            body_stall_error(url)
        }
    };
    loop {
        let wait = BODY_CHUNK_TIMEOUT.min(deadline.remaining());
        if wait.is_zero() {
            return Err(deadline.timeout_error(FetchPhase::Body, url));
        }
        match rx.recv_timeout(wait) {
            Ok(BodyReadEvent::Chunk(chunk)) => {
                let remaining = (MAX_RESPONSE_BYTES - total) as usize;
                chunks.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
                total += chunk.len() as u64;
                if total > MAX_RESPONSE_BYTES {
                    return Ok((chunks, true));
                }
            }
            Ok(BodyReadEvent::Done) => return Ok((chunks, false)),
            Ok(BodyReadEvent::Error(kind, message, waited)) => {
                return Err(body_read_failure(
                    deadline,
                    url,
                    kind,
                    &detail(&message),
                    waited,
                ));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => return Err(slow_body_error()),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(UrlFetchError::new(format!(
                    "Failed to read response body for {url}: body reader stopped unexpectedly"
                )));
            }
        }
    }
}

/// How a failed body read is reported. It is a body-phase timeout when the
/// fetch deadline has passed or the read had waited at least
/// [`BODY_CHUNK_TIMEOUT`], whatever its kind (see [`read_response_body`]);
/// the kinds `TimedOut` and `WouldBlock` also count as a body stall.
fn body_read_failure(
    deadline: &FetchDeadline,
    url: &str,
    kind: io::ErrorKind,
    message: &str,
    waited: Duration,
) -> UrlFetchError {
    if deadline.expired() {
        return deadline.timeout_error(FetchPhase::Body, url);
    }
    if waited >= BODY_CHUNK_TIMEOUT || is_body_stall_kind(kind) {
        return body_stall_error(url);
    }
    if message.is_empty() {
        return UrlFetchError::new(format!("Failed to read response body for {url}"));
    }
    UrlFetchError::new(format!("Failed to read response body for {url}: {message}"))
}

fn body_stall_error(url: &str) -> UrlFetchError {
    UrlFetchError::timeout(
        FetchPhase::Body,
        format!(
            "Body read stalled (no data for {}ms) fetching {url}",
            BODY_CHUNK_TIMEOUT.as_millis()
        ),
    )
}

fn is_body_stall_kind(kind: io::ErrorKind) -> bool {
    matches!(kind, io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock)
}

fn atomic_write(
    final_path: &Path,
    bytes: &[u8],
    options: &UrlFetchOptions,
) -> Result<(), UrlFetchError> {
    let parent = final_path.parent().unwrap_or_else(|| Path::new("."));
    crate::private_storage::create_dir_all(parent).map_err(|error| {
        UrlFetchError::new(format!(
            "Failed to create URL cache parent {}: {error}",
            parent.display()
        ))
    })?;

    let file_name = final_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            UrlFetchError::new(format!("Invalid cache path: {}", final_path.display()))
        })?;
    let tmp_path = final_path.with_file_name(format!(
        "{file_name}.tmp-{}-{}",
        std::process::id(),
        random_nonce()
    ));

    let write_result = (|| -> io::Result<()> {
        let mut file = crate::private_storage::create(&tmp_path)?;
        file.write_all(bytes)?;
        file.flush()?;
        Ok(())
    })();
    if let Err(error) = write_result {
        let _ = fs::remove_file(&tmp_path);
        return Err(UrlFetchError::new(format!(
            "Failed to write URL cache temp file {}: {error}",
            tmp_path.display()
        )));
    }

    if let Some(observer) = &options.atomic_write_observer {
        observer(&tmp_path, final_path);
    }

    fs::rename(&tmp_path, final_path).map_err(|error| {
        let _ = fs::remove_file(&tmp_path);
        UrlFetchError::new(format!(
            "Failed to finalize URL cache file {}: {error}",
            final_path.display()
        ))
    })
}

fn random_nonce() -> String {
    let mut bytes = [0u8; 8];
    if getrandom::fill(&mut bytes).is_err() {
        let fallback = now_ms() ^ u64::from(std::process::id());
        bytes = fallback.to_le_bytes();
    }
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn reqwest_error_detail(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        return format!("timeout: {error}");
    }
    if let Some(source) = error.source() {
        return format!("{source}");
    }
    error.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use url::Url;

    fn write_cache_entry(storage: &Path, hash: &str, url: &str, fetched_at: u64) -> PathBuf {
        let dir = cache_dir(storage);
        fs::create_dir_all(&dir).unwrap();
        let meta = CacheMeta {
            url: url.to_string(),
            content_type: "text/markdown".to_string(),
            extension: ".md".to_string(),
            fetched_at,
            download_truncated: false,
            downloaded_bytes: 4,
            total_bytes: Some(4),
        };
        fs::write(content_path(storage, hash, ".md"), "body").unwrap();
        let meta_file = meta_path(storage, hash);
        fs::write(&meta_file, serde_json::to_vec(&meta).unwrap()).unwrap();
        meta_file
    }

    fn cleanup_counting_reads(storage: &Path, now: u64) -> (usize, usize) {
        CLEANUP_META_READS.with(|reads| reads.set(0));
        let removed = cleanup_url_cache_at(storage, now).unwrap();
        (removed, CLEANUP_META_READS.with(std::cell::Cell::get))
    }

    /// Every configure sweeps the URL cache, so a sweep must not reopen
    /// metadata files that have not changed since the previous sweep.
    #[test]
    fn url_cache_cleanup_does_not_reread_unchanged_metadata() {
        let storage = tempfile::tempdir().unwrap();
        let now = 10 * CACHE_TTL_MS;
        for index in 0..3 {
            write_cache_entry(
                storage.path(),
                &format!("fresh{index}"),
                "https://example.com/a",
                now - 1_000,
            );
        }
        let stale = write_cache_entry(
            storage.path(),
            "stale",
            "https://example.com/b",
            now - CACHE_TTL_MS - 1,
        );

        assert_eq!(cleanup_counting_reads(storage.path(), now), (1, 4));
        assert!(!stale.exists());
        assert!(!content_path(storage.path(), "stale", ".md").exists());

        // Nothing changed: no metadata file is opened again.
        assert_eq!(cleanup_counting_reads(storage.path(), now), (0, 0));
    }

    /// A remembered fetch time is still compared with the current time, and a
    /// rewritten metadata file is read again, so the memo never keeps an entry
    /// alive past its TTL.
    #[test]
    fn url_cache_cleanup_memo_still_expires_and_notices_rewrites() {
        let storage = tempfile::tempdir().unwrap();
        let now = 10 * CACHE_TTL_MS;
        let kept = write_cache_entry(storage.path(), "kept", "https://example.com/a", now);
        let rewritten =
            write_cache_entry(storage.path(), "rewritten", "https://example.com/b", now);
        assert_eq!(cleanup_counting_reads(storage.path(), now), (0, 2));

        // A different size is a different version even within one mtime tick.
        write_cache_entry(
            storage.path(),
            "rewritten",
            "https://example.com/b-longer",
            now - CACHE_TTL_MS - 1,
        );
        assert_eq!(cleanup_counting_reads(storage.path(), now), (1, 1));
        assert!(!rewritten.exists());
        assert!(kept.exists());

        // The unchanged file expires from its remembered fetch time.
        assert_eq!(
            cleanup_counting_reads(storage.path(), now + CACHE_TTL_MS + 1),
            (1, 0)
        );
        assert!(!kept.exists());
    }

    #[test]
    fn web_view_urls_rewrite_to_raw_content() {
        let cases = [
            (
                "https://github.com/owner/repo/blob/main/docs/guide.md?download=1#L42",
                "https://raw.githubusercontent.com/owner/repo/main/docs/guide.md?download=1",
            ),
            (
                "https://github.com/owner/repo/raw/v1.2/src/lib.rs?raw=1#L7",
                "https://raw.githubusercontent.com/owner/repo/v1.2/src/lib.rs?raw=1",
            ),
            (
                "https://www.github.com/owner/repo/blob/main/README.md",
                "https://raw.githubusercontent.com/owner/repo/main/README.md",
            ),
            (
                "https://gitlab.com/owner/repo/-/blob/main/docs/guide.md?plain=1#L4",
                "https://gitlab.com/owner/repo/-/raw/main/docs/guide.md?plain=1",
            ),
        ];

        for (input, expected) in cases {
            let url = Url::parse(input).unwrap();
            assert_eq!(rewrite_url_for_fetch(&url).as_str(), expected);
        }
    }

    #[test]
    fn non_web_view_urls_are_not_rewritten() {
        for input in [
            "https://gist.github.com/owner/abcdef/blob/main/file.md",
            "https://github.com/owner/repo",
            "https://github.com/owner/repo/blob",
            "https://api.github.com/repos/owner/repo",
            "https://notgithub.com/owner/repo/blob/main/file.md",
            "https://gitlab.example.com/owner/repo/-/blob/main/file.md",
        ] {
            let url = Url::parse(input).unwrap();
            assert_eq!(rewrite_url_for_fetch(&url).as_str(), input);
        }
    }

    #[test]
    fn extension_from_path_uses_parser_mapping() {
        let url = Url::parse("https://example.com/pkg/index.mjs").unwrap();
        let (ext, from_path) = resolve_fetch_extension(&url, "text/javascript").unwrap();
        assert_eq!(ext, ".mjs");
        assert!(from_path);
    }

    #[test]
    fn text_plain_rs_url_ignores_content_type_gate() {
        let url = Url::parse("https://raw.githubusercontent.com/o/r/main/lib.rs").unwrap();
        let (ext, from_path) = resolve_fetch_extension(&url, "text/plain").unwrap();
        assert_eq!(ext, ".rs");
        assert!(from_path);
    }

    #[test]
    fn extensionless_javascript_maps_to_js() {
        let url = Url::parse("https://cdn.example/bundle").unwrap();
        let (ext, from_path) = resolve_fetch_extension(&url, "text/javascript").unwrap();
        assert_eq!(ext, ".js");
        assert!(!from_path);
    }

    #[test]
    fn extensionless_plain_stays_md() {
        let url = Url::parse("https://example.com/readme").unwrap();
        let (ext, _) = resolve_fetch_extension(&url, "text/plain").unwrap();
        assert_eq!(ext, ".md");
    }

    #[test]
    fn query_and_fragment_do_not_break_path_extension() {
        let url = Url::parse("https://example.com/src/file.ts?v=2#L10").unwrap();
        let (ext, from_path) = resolve_fetch_extension(&url, "text/plain").unwrap();
        assert_eq!(ext, ".ts");
        assert!(from_path);
    }

    #[test]
    fn percent_encoded_path_segment() {
        let url = Url::parse("https://example.com/foo%2Fbar.rs").unwrap();
        // path is /foo%2Fbar.rs - segment is foo%2Fbar.rs -> decode to foo/bar.rs -> ext rs
        let (ext, _) = resolve_fetch_extension(&url, "text/plain").unwrap();
        assert_eq!(ext, ".rs");
    }

    #[test]
    fn binary_sniff_detects_nul() {
        let mut body = vec![b'f', b'n', 0, b' '];
        assert!(body_contains_nul_in_prefix(&body));
        body = vec![b'h'; 9000];
        assert!(!body_contains_nul_in_prefix(&body));
    }

    #[test]
    fn unsupported_pdf_still_errors_via_resolve() {
        let url = Url::parse("https://example.com/doc.pdf").unwrap();
        assert!(resolve_fetch_extension(&url, "application/pdf").is_none());
    }

    #[test]
    fn is_permalink_anchor_doc_anchor_class_drops_anchor() {
        let md = html_to_markdown_converter()
            .convert("<a class=\"doc-anchor\" href=\"#stdio-transport\">x</a>")
            .unwrap();
        assert_eq!(
            md.trim(),
            "",
            "doc-anchor permalink should be dropped: {md:?}"
        );
    }

    #[test]
    fn html_to_markdown_rustdoc_heading_cleanup() {
        let html = "<main><h1>Function <span class=\"fn\">stdio</span>&nbsp;<button id=\"copy-path\">Copy item path</button></h1><h2 class=\"location\"><a href=\"#\">stdio</a></h2><h2><a class=\"doc-anchor\" href=\"#stdio-transport\">§</a>StdIO Transport</h2></main>";
        let md = html_to_markdown_converter().convert(html).unwrap();
        assert!(md.contains("# Function stdio"), "{md:?}");
        assert!(!md.contains("Copy item path"), "{md:?}");
        assert!(md.contains("## stdio"), "{md:?}");
        assert!(!md.contains("[stdio](#)"), "{md:?}");
        assert!(md.contains("## StdIO Transport"), "{md:?}");
        assert!(!md.contains('§'), "{md:?}");
        assert!(!md.contains("](#stdio-transport)"), "{md:?}");
    }

    #[test]
    fn html_to_markdown_docusaurus_hash_link_still_dropped() {
        let html = "<h2>The Retain Pipeline<a class=\"hash-link\" href=\"#x\" aria-label=\"Direct link to The Retain Pipeline\">\u{200b}</a></h2>";
        let md = html_to_markdown_converter().convert(html).unwrap();
        assert!(md.contains("## The Retain Pipeline"), "{md:?}");
        assert!(!md.contains('\u{200b}'), "{md:?}");
    }

    const CHROME_AUTO_CONNECT_HTML: &str =
        include_str!("../tests/fixtures/url_html/chrome_auto_connect.html");

    /// The fenced blocks of `markdown` as (opening fence line, body). Body
    /// lines lose only the indentation the opening fence has (a list item's
    /// indent), so the code's own indentation is compared as written.
    fn fenced_blocks(markdown: &str) -> Vec<(String, String)> {
        let mut blocks = Vec::new();
        let mut open: Option<(String, usize, Vec<String>)> = None;
        for line in markdown.lines() {
            let trimmed = line.trim_start();
            match open.take() {
                None => {
                    if trimmed.starts_with("```") {
                        let indent = line.len() - trimmed.len();
                        open = Some((trimmed.to_string(), indent, Vec::new()));
                    }
                }
                Some((fence_line, indent, mut body)) => {
                    let fence: String = fence_line.chars().take_while(|c| *c == '`').collect();
                    if trimmed == fence {
                        blocks.push((fence_line, body.join("\n")));
                    } else {
                        let unindented = line
                            .strip_prefix(&" ".repeat(indent))
                            .unwrap_or(line.trim_start());
                        body.push(unindented.to_string());
                        open = Some((fence_line, indent, body));
                    }
                }
            }
        }
        assert!(open.is_none(), "unclosed fence in {markdown}");
        blocks
    }

    #[test]
    fn html_to_markdown_pre_closed_code_early_stays_one_fenced_block() {
        // The live page closes </code> inside the "chrome-devtools-mcp@latest"
        // string; everything after it must still land inside the same fence.
        let md = html_to_markdown_converter()
            .convert(CHROME_AUTO_CONNECT_HTML)
            .unwrap();
        let blocks = fenced_blocks(&md);
        assert_eq!(blocks.len(), 1, "{md}");
        let (fence_line, body) = &blocks[0];
        assert_eq!(fence_line, "```", "{md}");
        assert_eq!(
            body,
            concat!(
                "{\n",
                "  \"mcpServers\": {\n",
                "    \"chrome-devtools\": {\n",
                "      \"command\": \"npx\",\n",
                "      \"args\": [\"chrome-devtools-mcp@latest\", \"--autoConnect\"]\n",
                "    }\n",
                "  }\n",
                "}",
            ),
            "{md}"
        );
        assert!(
            md.contains(r#""args": ["chrome-devtools-mcp@latest", "--autoConnect"]"#),
            "{md}"
        );
        assert!(!md.contains("```ome"), "{md}");
    }

    #[test]
    fn html_to_markdown_pre_keeps_code_verbatim() {
        let html = "<pre><code class=\"language-md\">line *one*\n\n\n\n  # not a heading &lt;b&gt;\n</code></pre>";
        let md = html_to_markdown_converter().convert(html).unwrap();
        assert_eq!(
            md.trim(),
            "```md\nline *one*\n\n\n\n  # not a heading <b>\n```",
            "{md:?}"
        );
    }

    #[test]
    fn html_to_markdown_code_containing_triple_backticks_gets_longer_fence() {
        let html = "<pre><code>Example:\n```rust\nfn main() {}\n```\n</code></pre>";
        let md = html_to_markdown_converter().convert(html).unwrap();
        assert_eq!(
            md.trim(),
            "````\nExample:\n```rust\nfn main() {}\n```\n````",
            "{md:?}"
        );

        let html = "<pre><code>a ````` run</code></pre>";
        let md = html_to_markdown_converter().convert(html).unwrap();
        assert_eq!(md.trim(), "``````\na ````` run\n``````", "{md:?}");
    }

    #[test]
    fn html_to_markdown_pre_without_code_keeps_htmd_rendering() {
        let html = "<pre>plain preformatted</pre>";
        let md = html_to_markdown_converter().convert(html).unwrap();
        assert!(!md.contains("```"), "{md:?}");
        assert!(md.contains("plain preformatted"), "{md:?}");
    }

    #[test]
    fn html_to_markdown_pre_per_line_divs_keep_their_lines() {
        let md = html_to_markdown_converter()
            .convert(include_str!(
                "../tests/fixtures/url_html/per_line_divs.html"
            ))
            .unwrap();
        let blocks = fenced_blocks(&md);
        assert_eq!(blocks.len(), 1, "{md}");
        // The template literal's single backticks leave the fence at three.
        assert_eq!(blocks[0].0, "```js", "{md}");
        assert_eq!(
            blocks[0].1, "function greet(name) {\n  return `Hello, ${name}`;\n\n}",
            "{md}"
        );
    }

    #[test]
    fn html_to_markdown_pre_table_rows_keep_their_lines() {
        let md = html_to_markdown_converter()
            .convert(include_str!("../tests/fixtures/url_html/table_rows.html"))
            .unwrap();
        let blocks = fenced_blocks(&md);
        assert_eq!(blocks.len(), 1, "{md}");
        assert_eq!(blocks[0].0, "```python", "{md}");
        assert_eq!(
            blocks[0].1, "def add(a, b):\n    return a + b\n\nprint(add(1, 2))",
            "{md}"
        );
    }

    #[test]
    fn html_to_markdown_pre_shiki_line_spans_keep_their_lines() {
        let md = html_to_markdown_converter()
            .convert(include_str!("../tests/fixtures/url_html/shiki_lines.html"))
            .unwrap();
        let blocks = fenced_blocks(&md);
        assert_eq!(blocks.len(), 1, "{md}");
        assert_eq!(blocks[0].0, "```", "{md}");
        assert_eq!(
            blocks[0].1,
            "const total = items.reduce((sum, item) => sum + item.price, 0);\n\nconsole.log(total);",
            "{md}"
        );
    }

    #[test]
    fn html_to_markdown_pre_list_item_lines_keep_their_lines() {
        let html = "<pre><code><ol><li>first</li><li></li><li>  third</li></ol></code></pre>";
        let md = html_to_markdown_converter().convert(html).unwrap();
        assert_eq!(md.trim(), "```\nfirst\n\n  third\n```", "{md:?}");
    }

    #[test]
    fn html_to_markdown_pre_newline_text_between_lines_is_not_doubled() {
        // Lines that already end in newline text must not gain a second break.
        let html = "<pre><code><span class=\"line\">a</span>\n<span class=\"line\">b</span>\n<div>c\n</div></code></pre>";
        let md = html_to_markdown_converter().convert(html).unwrap();
        assert_eq!(md.trim(), "```\na\nb\nc\n```", "{md:?}");
    }

    #[test]
    fn code_fence_for_outgrows_longest_backtick_run() {
        assert_eq!(code_fence_for("no ticks"), "```");
        assert_eq!(code_fence_for("`inline` ``double``"), "```");
        assert_eq!(code_fence_for("```"), "````");
        assert_eq!(code_fence_for("``` and ````"), "`````");
    }

    /// htmd decodes an HTML entity into its character and then escapes that
    /// character for Markdown, so the converted text spells a key in a form
    /// the byte matchers alone do not recognise.
    #[test]
    fn html_conversion_markdown_escapes_an_entity_decoded_character() {
        let markdown =
            convert_html_body_to_markdown(b"<p>abcdef&#42;1234567890</p>", "https://example.test/")
                .unwrap();
        let text = String::from_utf8(markdown).unwrap();
        assert!(text.contains("abcdef\\*1234567890"), "{text}");
    }

    /// The text of a failed body read can name the request URL, which held
    /// the key in a placeholder fetch; it is scrubbed before it is reported.
    #[test]
    fn a_failed_body_read_in_a_placeholder_fetch_is_scrubbed_of_held_keys() {
        struct Failing;
        impl Read for Failing {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other(
                    "error reading body for url (https://api.test/v1?key=fetch-secret-key%2F0123456789%2Babc)",
                ))
            }
        }
        let mut held = HeldKeys::default();
        held.hold("test", b"fetch-secret-key/0123456789+abc")
            .unwrap();
        let deadline = FetchDeadline::for_current_call();
        let sanitize = |detail: &str| held.sanitize_detail(detail);
        let error = read_response_body_with(Failing, "https://api.test/v1", &deadline, &sanitize)
            .expect_err("the read fails");
        let text = error.to_string();
        assert!(!text.contains("fetch-secret-key"), "{text}");
        assert!(text.contains("key={{ck-secret:test}})"), "{text}");
    }

    /// reqwest prints the request URL in its own error text; a placeholder
    /// fetch's failure must drop it (the URL held the key), not merely scrub
    /// the key out of it.
    #[test]
    fn a_placeholder_fetch_send_failure_drops_the_request_url() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            // Accept one connection and hold it open without answering.
            let accepted = listener.accept();
            thread::sleep(Duration::from_secs(5));
            drop(accepted);
        });
        let url = format!("http://{addr}/v1?key=fetch-secret-key%2F0123456789%2Babc");
        let error = Client::builder()
            .timeout(Duration::from_millis(300))
            .build()
            .unwrap()
            .get(&url)
            .send()
            .expect_err("the silent server times the request out");
        assert!(error.to_string().contains("for url"), "{error}");
        let mut held = HeldKeys::default();
        held.hold("test", b"fetch-secret-key/0123456789+abc")
            .unwrap();
        let deadline = FetchDeadline::for_current_call();
        let failure = send_failure(
            error,
            Some(FetchPhase::Headers),
            &deadline,
            "http://api.test/v1?key={{ck-secret:test}}",
            Some(&held),
        );
        let text = failure.to_string();
        assert!(text.contains("timed out"), "{text}");
        assert!(!text.contains("for url"), "{text}");
        assert!(!text.contains("fetch-secret-key"), "{text}");
    }

    #[test]
    fn deprecated_site_local_ipv6_is_private() {
        for address in ["fec0::1", "feff::1", "fef0:1::1"] {
            let ip: IpAddr = address.parse().unwrap();
            assert!(is_private_ip(ip), "{address}");
        }
        assert!(!is_private_ip("2606:4700::1".parse().unwrap()));
    }
}

#[cfg(test)]
#[path = "url_fetch_deadline_tests.rs"]
mod deadline_tests;
