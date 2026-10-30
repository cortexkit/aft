//! A command-private CONNECT proxy. The launcher supplies only scanned, resolved
//! labels and arms the new command session before releasing the child. The job
//! owner retains the handle and reports process-tree end or the end of the
//! configured lifetime SecretScope, either of which stops the proxy.
//! The armed leader's start time pins its generation. Loss or replacement of
//! that leader also stops the proxy, even if orphaned descendants remain.
//! Each proxy admits at most 16 connections and 32 in-flight requests/streams.
//! Sockets have a 30-second activity-based idle timeout (also the TCP connect
//! deadline). Response coding depth is at most four. At most 32 label/key
//! matcher sets and 4096 bytes per material value are accepted; old matchers
//! are never evicted to make room. JSON input remains capped at 1 MiB and grows
//! only as body bytes arrive. Limits refuse new work rather than weaken scrub.
//! On an approved target, a header placeholder whose label was not granted at
//! launch refuses by label and host without fetching material. It is not added
//! to the command's substitution set. Other destinations remain literal.
//!
//! # Memory boundary
//!
//! Vault material, matcher patterns, overlap windows, and all proxy-created
//! material-bearing request staging use zeroizing owners. This includes the Bytes owners behind
//! substituted header values, query-bearing URIs, and JSON bodies. The rules
//! evaluator plans locations with public dummy bytes, never real material.
//!
//! Third-party codec buffers are outside that ownership guarantee: rustls owns
//! TLS plaintext/record buffers; h2 owns HTTP/2 stream and HPACK buffers; Hyper
//! owns its HTTP/1 encoder write buffer; async-compression and its codec crates
//! own decompression workspaces. These codecs allocate and manage their own
//! buffers rather than accepting AFT's zeroizing owners. The proxy does not
//! persist or log any of them, or format key-bearing requests or responses with
//! `Debug` or `Display`. Zeroization is defence in depth in the daemon, not a claim
//! that third-party protocol implementations erase every transient copy.
//!
//! # What the response scrub does not catch
//!
//! Responses are scrubbed as a stream with `Scrubber::push` and `finish`, held
//! back by a fixed window of `6k - 1` bytes for the longest installed key of
//! `k` bytes. Unlike a placeholder URL fetch, the proxy runs no residual check
//! over the finished response. A covered server, one this command was allowed
//! to send the key to, can therefore make these reach the command's stdout:
//!
//! - Base64 of a key split by spaces, tabs or line breaks whose span, from the
//!   first to the last character of the spelling, is longer than six times the
//!   longest key, when a read boundary falls inside that span. Shorter split
//!   spellings, and every other spelling in the scrubber's families (raw,
//!   `%XX`, form, JSON escapes, hex, contiguous base64), are always caught.
//! - Every spelling outside those families, in any chunking: nested
//!   percent-encoding (`%2530%2531...`), HTML character references, base64
//!   separated by vertical tab, form feed or punctuation (`MDEy.MzQ1...`),
//!   Markdown escapes, and a key split across markup.
//!
//! This is accepted: that server already holds the key, and catching these in
//! a stream would need a holdback with no bound, since such a spelling has no
//! length limit.

mod audit;
mod limits;
mod peer;
mod request;
mod response;
mod websocket;

use std::collections::BTreeSet;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use base64::Engine;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo};
use parking_lot::Mutex;
use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};
use secret_rules::location::{validate_header_name, LabelRule, Location};
use secret_rules::scrub::{MatcherSet, Scrubber};
use secret_rules::target::{canonical_host, ConnectAuthority};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use zeroize::Zeroizing;

use crate::secret::vault::SecretVault;
use crate::secret::SecretScope;

pub(crate) type Body = http_body_util::combinators::UnsyncBoxBody<Bytes, std::io::Error>;

/// Constant agent-visible wording; labels and hosts are appended as data.
pub const PROXY_REFUSAL: &str = "Secret proxy refused the request";

/// A launch-time resolution, not a request-time material cache.
#[derive(Clone, Debug)]
pub struct GrantedLabel {
    pub rule: LabelRule,
    pub expires_at_ms: u64,
}

pub struct ProxyConfig {
    pub vault: Arc<dyn SecretVault>,
    pub authorization_scope: SecretScope,
    pub lifetime_scope: SecretScope,
    pub scanned_labels: Vec<String>,
    pub grants: Vec<GrantedLabel>,
    pub origin: String,
}

/// Errors contain no transport diagnostics, URLs, credentials or vault text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyError {
    UnsupportedPlatform,
    InvalidRules,
    Initialization,
    InvalidSession,
    Stopped,
}

/// This protection is process-wide and intentionally irreversible. Resolution
/// adapters can transiently receive material, so every proxy owner must prevent
/// core dumps before starting a secret runtime.
pub(crate) fn protect_secret_process() -> Result<(), ProxyError> {
    // Test builds harden for real only inside the isolated child processes the
    // audit tests spawn. In a shared test process the change would outlive the
    // test that made it: on Linux a non-dumpable process can no longer read its
    // own /proc/self/io, which breaks unrelated tests that run afterwards.
    #[cfg(test)]
    {
        test_hardening::mark();
        if std::env::var_os(test_hardening::REAL_ENV).is_none() {
            return Ok(());
        }
    }
    #[cfg(unix)]
    {
        let limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        if unsafe { libc::setrlimit(libc::RLIMIT_CORE, &limit) } != 0 {
            return Err(ProxyError::Initialization);
        }
        #[cfg(target_os = "linux")]
        if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0 {
            return Err(ProxyError::Initialization);
        }
    }
    Ok(())
}

/// Test-build record of `protect_secret_process` calls, so fakes can check that
/// hardening ran before resolution without hardening the shared test process.
#[cfg(test)]
pub(crate) mod test_hardening {
    use std::cell::Cell;

    /// Set by the audit tests' isolated child processes; only there does
    /// `protect_secret_process` change the real process limits.
    pub(crate) const REAL_ENV: &str = "AFT_S4_AUDIT_CHILD";

    thread_local! {
        static HARDENED: Cell<bool> = const { Cell::new(false) };
    }

    pub(super) fn mark() {
        HARDENED.with(|hardened| hardened.set(true));
    }

    /// Whether `protect_secret_process` ran on this thread. Each test runs on
    /// its own thread, so one test's call never satisfies another's check.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) fn hardened_on_this_thread() -> bool {
        HARDENED.with(Cell::get)
    }
}

pub(crate) struct State {
    config: ProxyConfig,
    session: AtomicI32,
    session_birth: std::sync::atomic::AtomicU64,
    stopped: AtomicBool,
    token: Zeroizing<String>,
    expected_auth: Zeroizing<Vec<u8>>,
    ca: rcgen::Certificate,
    ca_key: KeyPair,
    upstream: rustls::ClientConfig,
    material_gate: Mutex<()>,
    connections: Arc<tokio::sync::Semaphore>,
    requests: Arc<tokio::sync::Semaphore>,
    // Keep each installed key as an exact-byte matcher for response scrubbers.
    // Rotation adds a matcher instead of replacing earlier key values.
    patterns: Mutex<Vec<(String, Zeroizing<Vec<u8>>)>>,
    #[cfg(test)]
    headers_sensitive: AtomicBool,
    #[cfg(test)]
    test_identity_generation: std::sync::atomic::AtomicU64,
    #[cfg(test)]
    test_idle_timeout_ms: std::sync::atomic::AtomicU64,
}

impl State {
    fn idle_timeout(&self) -> Duration {
        #[cfg(test)]
        {
            let ms = self.test_idle_timeout_ms.load(Ordering::Acquire);
            if ms != 0 {
                return Duration::from_millis(ms);
            }
        }
        limits::IDLE
    }
    fn generation_valid(&self) -> bool {
        let pid = self.session.load(Ordering::Acquire);
        if pid <= 0 {
            return false;
        }
        let current = peer::process_start_time(pid);
        #[cfg(test)]
        let current = {
            let changed = self.test_identity_generation.load(Ordering::Acquire);
            if changed != 0 {
                Some(changed)
            } else {
                current
            }
        };
        current.is_some_and(|birth| birth == self.session_birth.load(Ordering::Acquire))
    }

    fn mark_stopped(&self) {
        // Latch cancellation before waiting for a synchronous vault callback.
        // Its return must not authorize dispatch just because stop is waiting.
        self.stopped.store(true, Ordering::Release);
        let _gate = self.material_gate.lock();
    }

    fn scrubber(&self) -> Result<Scrubber, ()> {
        let patterns = self.patterns.lock();
        let sets = patterns
            .iter()
            .map(|(label, key)| MatcherSet::new(label, key).map_err(|_| ()))
            .collect::<Result<Vec<_>, _>>()?;
        Scrubber::new(sets).map_err(|_| ())
    }

    fn refresh(&self, scrubber: &mut Scrubber) -> Result<(), ()> {
        let patterns = self.patterns.lock();
        for (label, key) in &patterns[scrubber.matcher_count()..] {
            scrubber
                .install(MatcherSet::new(label, key).map_err(|_| ())?)
                .map_err(|_| ())?;
        }
        Ok(())
    }

    fn install(&self, label: &str, key: &[u8]) -> Result<(), ()> {
        if key.len() > limits::MATERIAL_BYTES {
            return Err(());
        }
        let mut patterns = self.patterns.lock();
        if self.stopped.load(Ordering::Acquire) {
            return Err(());
        }
        if patterns
            .iter()
            .any(|(l, k)| l == label && k.as_slice() == key)
        {
            return Ok(());
        }
        if patterns.len() >= limits::MATCHERS {
            return Err(());
        }
        let mut sets = patterns
            .iter()
            .map(|(l, k)| MatcherSet::new(l, k).map_err(|_| ()))
            .collect::<Result<Vec<_>, _>>()?;
        sets.push(MatcherSet::new(label, key).map_err(|_| ())?);
        Scrubber::new(sets).map_err(|_| ())?;
        patterns.push((label.to_owned(), Zeroizing::new(key.to_vec())));
        Ok(())
    }

    fn scrub(&self, bytes: &[u8]) -> Result<Vec<u8>, ()> {
        let mut scrubber = self.scrubber()?;
        let mut out = scrubber.push(bytes);
        out.extend(scrubber.finish());
        Ok(out)
    }

    fn tls_for(&self, host: &str) -> Result<rustls::ServerConfig, ()> {
        let host = network_host(host);
        let key = KeyPair::generate().map_err(|_| ())?;
        // Distinct issuer/leaf names and an issuing-key identifier keep clients
        // from treating the leaf as self-issued or choosing another bundle CA.
        let mut params = CertificateParams::new(vec![host.to_owned()]).map_err(|_| ())?;
        params.distinguished_name = rcgen::DistinguishedName::new();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, host);
        params.use_authority_key_identifier_extension = true;
        let cert = params
            .signed_by(&key, &self.ca, &self.ca_key)
            .map_err(|_| ())?;
        let der = Zeroizing::new(key.serialize_der());
        let private_key = rustls::pki_types::PrivatePkcs8KeyDer::from(der.to_vec()).into();
        let mut config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert.der().clone()], private_key)
            .map_err(|_| ())?;
        config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        Ok(config)
    }
}

/// Dropping or stopping the handle closes the listener and all connections.
/// Keep it in the background job, not in the launching tool call's stack frame.
pub struct SecretProxy {
    address: SocketAddr,
    ca_pem: String,
    state: Arc<State>,
    stop: watch::Sender<bool>,
    thread: Option<JoinHandle<()>>,
}

impl SecretProxy {
    #[cfg(all(test, unix))]
    pub(crate) fn start_for_test(
        config: ProxyConfig,
        upstream: rustls::ClientConfig,
    ) -> Result<Self, ProxyError> {
        Self::start_with_tls(config, upstream)
    }

    pub fn start(config: ProxyConfig) -> Result<Self, ProxyError> {
        protect_secret_process()?;
        let upstream =
            crate::platform_tls::client_config().map_err(|_| ProxyError::Initialization)?;
        Self::start_with_tls(config, upstream)
    }

    fn start_with_tls(
        config: ProxyConfig,
        upstream: rustls::ClientConfig,
    ) -> Result<Self, ProxyError> {
        protect_secret_process()?;
        if !cfg!(any(target_os = "linux", target_os = "macos")) {
            return Err(ProxyError::UnsupportedPlatform);
        }
        let scanned: BTreeSet<_> = config.scanned_labels.iter().collect();
        let resolved: BTreeSet<_> = config.grants.iter().map(|g| &g.rule.label).collect();
        if scanned.is_empty()
            || scanned != resolved
            || resolved.len() != config.grants.len()
            || config.grants.iter().any(|g| {
                let r = &g.rule;
                !secret_rules::placeholder::is_label(&r.label)
                    || canonical_host(&r.host).is_err()
                    || secret_rules::target::normalize_path_prefix(&r.path_prefix).map_or(true, |p| p != r.path_prefix)
                    || matches!(&r.location, Location::Header(name) if validate_header_name(name).is_err())
            })
        {
            return Err(ProxyError::InvalidRules);
        }
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut params =
            CertificateParams::new(Vec::new()).map_err(|_| ProxyError::Initialization)?;
        params.distinguished_name = rcgen::DistinguishedName::new();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "AFT per-command secret proxy CA");
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![
            rcgen::KeyUsagePurpose::KeyCertSign,
            rcgen::KeyUsagePurpose::DigitalSignature,
        ];
        let ca_key = KeyPair::generate().map_err(|_| ProxyError::Initialization)?;
        let ca = params
            .self_signed(&ca_key)
            .map_err(|_| ProxyError::Initialization)?;
        let ca_pem = ca.pem();
        let mut random = Zeroizing::new([0u8; 32]);
        getrandom::fill(&mut random[..]).map_err(|_| ProxyError::Initialization)?;
        let token =
            Zeroizing::new(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&random[..]));
        let expected_auth = basic_credentials(&token)?;
        let state = Arc::new(State {
            config,
            session: AtomicI32::new(0),
            session_birth: std::sync::atomic::AtomicU64::new(0),
            stopped: AtomicBool::new(false),
            token,
            expected_auth,
            ca,
            ca_key,
            upstream,
            material_gate: Mutex::new(()),
            connections: Arc::new(tokio::sync::Semaphore::new(limits::CONNECTIONS)),
            requests: Arc::new(tokio::sync::Semaphore::new(limits::REQUESTS)),
            patterns: Mutex::new(Vec::new()),
            #[cfg(test)]
            headers_sensitive: AtomicBool::new(false),
            #[cfg(test)]
            test_identity_generation: std::sync::atomic::AtomicU64::new(0),
            #[cfg(test)]
            test_idle_timeout_ms: std::sync::atomic::AtomicU64::new(0),
        });
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .map_err(|_| ProxyError::Initialization)?;
        listener
            .set_nonblocking(true)
            .map_err(|_| ProxyError::Initialization)?;
        let address = listener
            .local_addr()
            .map_err(|_| ProxyError::Initialization)?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|_| ProxyError::Initialization)?;
        let (stop, receiver) = watch::channel(false);
        let shared = state.clone();
        let thread = std::thread::Builder::new()
            .name("secret-proxy".into())
            .spawn(move || {
                runtime.block_on(serve(listener, shared.clone(), receiver));
                shared.mark_stopped();
                runtime.shutdown_timeout(Duration::from_secs(1));
                shared.patterns.lock().clear();
            })
            .map_err(|_| ProxyError::Initialization)?;
        Ok(Self {
            address,
            ca_pem,
            state,
            stop,
            thread: Some(thread),
        })
    }

    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// Exec-time only: never persist the returned credential with a task request.
    pub fn proxy_url(&self) -> Zeroizing<String> {
        let address = self.address.to_string();
        let mut url = Zeroizing::new(String::with_capacity(
            9 + self.state.token.len() + address.len(),
        ));
        url.push_str("http://");
        url.push_str(&self.state.token);
        url.push_str(":@");
        url.push_str(&address);
        url
    }

    /// Public CA certificate only; the signer stays in daemon memory.
    pub fn ca_pem(&self) -> &str {
        &self.ca_pem
    }

    /// Arm only a newly created session leader, while the child is held at its
    /// exec barrier. Connections handled while no session is armed are refused.
    pub fn arm_command(&self, pid: u32) -> Result<(), ProxyError> {
        let pid = i32::try_from(pid).map_err(|_| ProxyError::InvalidSession)?;
        let _gate = self.state.material_gate.lock();
        if self.state.stopped.load(Ordering::Acquire) {
            return Err(ProxyError::Stopped);
        }
        if !peer::is_session_leader(pid) || self.state.session.load(Ordering::Acquire) != 0 {
            return Err(ProxyError::InvalidSession);
        }
        let birth = peer::process_start_time(pid).ok_or(ProxyError::InvalidSession)?;
        self.state.session_birth.store(birth, Ordering::Release);
        self.state.session.store(pid, Ordering::Release);
        Ok(())
    }

    pub fn stop_if_scope_ended(&mut self, scope: &SecretScope) {
        if &self.state.config.lifetime_scope == scope {
            self.stop();
        }
    }

    /// The job's process-tree owner calls this even if a descendant escaped its
    /// original session; session disappearance is also watched defensively.
    pub fn process_tree_ended(&mut self) {
        self.stop();
    }

    pub fn stop(&mut self) {
        self.state.mark_stopped();
        let _ = self.stop.send(true);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        self.state.patterns.lock().clear();
    }
}

impl Drop for SecretProxy {
    fn drop(&mut self) {
        self.stop();
    }
}

async fn serve(
    listener: std::net::TcpListener,
    state: Arc<State>,
    mut stop: watch::Receiver<bool>,
) {
    let Ok(listener) = TcpListener::from_std(listener) else {
        return;
    };
    let mut connections = tokio::task::JoinSet::new();
    let mut monitor = tokio::time::interval(Duration::from_millis(100));
    loop {
        tokio::select! {
            biased;
            _ = stop.changed() => break,
            _ = monitor.tick() => {
                let session = state.session.load(Ordering::Acquire);
                if session > 0 && (!state.generation_valid() || !peer::session_alive(session)) { break; }
            }
            accepted = listener.accept() => {
                let Ok((socket, _)) = accepted else { break; };
                let Ok(lease) = state.connections.clone().try_acquire_owned() else { continue; };
                let lease = Arc::new(lease);
                let state = state.clone();
                connections.spawn(async move {
                    let session = state.session.load(Ordering::Acquire);
                    if session <= 0 || !state.generation_valid() || !peer::authorized(&socket, session) || !state.generation_valid() { return; }
                    let socket = limits::IdleIo::new(socket, state.idle_timeout());
                    let service = service_fn(move |request| connect(request, state.clone(), lease.clone()));
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(socket), service).with_upgrades().await;
                });
            }
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
        }
    }
    state.mark_stopped();
    connections.abort_all();
    while connections.join_next().await.is_some() {}
}

async fn connect(
    mut request: Request<Incoming>,
    state: Arc<State>,
    lease: Arc<tokio::sync::OwnedSemaphorePermit>,
) -> Result<Response<Body>, std::convert::Infallible> {
    let refused = || refusal("", "");
    if request.method() != Method::CONNECT || state.stopped.load(Ordering::Acquire) {
        return Ok(refused());
    }
    let mut credentials = request.headers().get_all("proxy-authorization").iter();
    if credentials.next().map(|v| v.as_bytes()) != Some(state.expected_auth.as_slice())
        || credentials.next().is_some()
    {
        return Ok(refused());
    }
    let Some(authority) = request.uri().authority() else {
        return Ok(refused());
    };
    let Ok(authority) = ConnectAuthority::parse(authority.as_str()) else {
        return Ok(refused());
    };
    if request.headers().get_all("host").iter().any(|h| {
        h.to_str().ok().is_none_or(|h| {
            secret_rules::target::authority_matches_connect(&authority, Some(h), None, "/").is_err()
        })
    }) {
        return Ok(refused());
    }
    let intercepted = state
        .config
        .grants
        .iter()
        .any(|g| canonical_host(&g.rule.host).ok().as_ref() == Some(&authority.host));
    let tls = if intercepted {
        match state.tls_for(&authority.host) {
            Ok(tls) => Some(tls),
            Err(()) => return Ok(refused()),
        }
    } else {
        None
    };
    let upstream = if intercepted {
        None
    } else {
        match tokio::time::timeout(
            state.idle_timeout(),
            TcpStream::connect((network_host(&authority.host), authority.port)),
        )
        .await
        {
            Ok(Ok(socket)) => Some(limits::IdleIo::new(socket, state.idle_timeout())),
            _ => return Ok(refused()),
        }
    };
    tokio::spawn(async move {
        let _lease = lease;
        let Ok(upgraded) = hyper::upgrade::on(&mut request).await else {
            return;
        };
        let mut io = TokioIo::new(upgraded);
        if let Some(mut upstream) = upstream {
            let _ = tokio::io::copy_bidirectional(&mut io, &mut upstream).await;
        } else if let Some(tls) = tls {
            let Ok(tls) = tokio_rustls::TlsAcceptor::from(Arc::new(tls))
                .accept(io)
                .await
            else {
                return;
            };
            let service = service_fn(move |request| {
                request::forward(request, authority.clone(), state.clone())
            });
            let _ = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
                .serve_connection_with_upgrades(TokioIo::new(tls), service)
                .await;
        }
    });
    Ok(Response::new(full(Bytes::new())))
}

fn basic_credentials(token: &str) -> Result<Zeroizing<Vec<u8>>, ProxyError> {
    let mut userinfo = Zeroizing::new(Vec::with_capacity(token.len() + 1));
    userinfo.extend_from_slice(token.as_bytes());
    userinfo.push(b':');
    let encoded = base64::encoded_len(userinfo.len(), true).ok_or(ProxyError::Initialization)?;
    let mut header = Zeroizing::new(vec![0u8; 6 + encoded]);
    header[..6].copy_from_slice(b"Basic ");
    base64::engine::general_purpose::STANDARD
        .encode_slice(&userinfo[..], &mut header[6..])
        .map_err(|_| ProxyError::Initialization)?;
    Ok(header)
}

pub(crate) fn full(bytes: Bytes) -> Body {
    Full::new(bytes)
        .map_err(|never| match never {})
        .boxed_unsync()
}

struct SecretBytes(Zeroizing<Vec<u8>>);

impl AsRef<[u8]> for SecretBytes {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

pub(crate) fn secret_bytes(bytes: Zeroizing<Vec<u8>>) -> Bytes {
    Bytes::from_owner(SecretBytes(bytes))
}

// Brackets belong to an HTTP authority, not an IP SAN, socket lookup or TLS name.
pub(super) fn network_host(host: &str) -> &str {
    host.strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host)
}

pub(crate) fn refusal(label: &str, host: &str) -> Response<Body> {
    let mut response = Response::new(full(Bytes::from(format!(
        "{PROXY_REFUSAL}: {label} ({host})\n"
    ))));
    *response.status_mut() = StatusCode::FORBIDDEN;
    response.headers_mut().insert(
        "content-type",
        hyper::header::HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response
}

#[cfg(all(test, unix))]
mod tests;
