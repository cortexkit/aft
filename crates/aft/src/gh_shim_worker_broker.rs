//! Purpose-separated, command-lifetime ingress for jailed GitHub reads.
//!
//! The child controls every byte. Neither the ordinary relay ticket nor a
//! caller-selected identity is accepted here; the general relay is unreachable.

#[cfg(unix)]
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(unix)]
use std::sync::{LazyLock, Mutex};
use std::time::Duration;
#[cfg(unix)]
use std::time::Instant;

use serde::{Deserialize, Serialize};
#[cfg(unix)]
use serde_json::json;
use serde_json::Value;
#[cfg(unix)]
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
#[cfg(unix)]
use tokio::sync::{watch, Semaphore};

#[cfg(unix)]
use crate::gh_shim_relay::{self as relay, RelayReply};
use crate::gh_shim_relay::{GhRelay, ScopeStamp};

pub(crate) const SOCKET_ENV: &str = "AFT_GH_READ_SOCKET";
pub(crate) const CAPABILITY_FILE_ENV: &str = "AFT_GH_READ_CAPABILITY_FILE";
pub(crate) const STATE_ENV: &str = "AFT_GH_SHIM_STATE_DIR";
pub(crate) const CHILD_KEYS: [&str; 5] = [
    crate::gh_shim_ticket::GH_SHIM_TICKET_ENV,
    crate::gh_shim_ticket::GH_SHIM_WORKER_ENV,
    SOCKET_ENV,
    CAPABILITY_FILE_ENV,
    STATE_ENV,
];
#[cfg(unix)]
const MAX_FRAME: usize = 65_536;
#[cfg(unix)]
const MAX_RESPONSE: usize = 2_097_152;
// A UTF-8 body byte can expand to six JSON bytes (for example, NUL becomes
// \u0000). Leave one request frame for the typed result's metadata and wrapper.
#[cfg(unix)]
const MAX_ENCODED_RESPONSE: usize =
    crate::gh_shim::read_contract::MAX_REPLY_BYTES as usize * 6 + MAX_FRAME;
#[cfg(unix)]
const FRAME_DEADLINE: Duration = Duration::from_secs(2);
#[cfg(unix)]
const CALL_DEADLINE: Duration = Duration::from_secs(20);
#[cfg(unix)]
const CONNECTIONS: usize = 4;
#[cfg(unix)]
static GLOBAL: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(64)));

/// Serde's closed enum rejects unknown operations and authority-bearing fields
/// before registry access. A connection contains exactly one length-prefixed JSON
/// envelope; it is not a SUBC connection and has no subscription protocol.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, tag = "op")]
pub(crate) enum Operation {
    #[serde(rename = "gh_shim.session_context")]
    SessionContext,
    #[serde(rename = "gh_shim.read_request")]
    ReadRequest {
        request: crate::gh_shim::read_contract::Request,
    },
}

#[cfg(unix)]
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    capability: String,
    operation: Operation,
}

pub(crate) struct ScopeQuery {
    pub session: String,
    pub reply: oneshot::Sender<Option<ScopeStamp>>,
}

#[cfg(unix)]
#[derive(Clone)]
struct Service {
    relay: Arc<GhRelay>,
    scopes: mpsc::Sender<ScopeQuery>,
    runtime: tokio::runtime::Handle,
    endpoints: Result<Arc<crate::gh_shim_worker_endpoints::Generation>, String>,
    connection_slots: Arc<Semaphore>,
}
#[cfg(unix)]
static SERVICE: Mutex<Option<Service>> = Mutex::new(None);

#[cfg(all(test, unix))]
thread_local! {
    static TEST_SERVICE: std::cell::RefCell<Option<Option<Service>>> = const { std::cell::RefCell::new(None) };
}

#[cfg(unix)]
fn command_service() -> Option<Service> {
    #[cfg(test)]
    if let Some(service) = TEST_SERVICE.with(|slot| slot.borrow().clone()) {
        return service;
    }
    SERVICE.lock().unwrap_or_else(|p| p.into_inner()).clone()
}

#[cfg(all(test, unix))]
fn with_service_for_test<R>(service: Option<Service>, run: impl FnOnce() -> R) -> R {
    struct Restore(Option<Option<Service>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            TEST_SERVICE.with(|slot| slot.replace(self.0.take()));
        }
    }
    let _restore = Restore(TEST_SERVICE.with(|slot| slot.replace(Some(service))));
    run()
}

#[cfg(all(test, unix))]
pub(crate) fn without_service_for_test<R>(run: impl FnOnce() -> R) -> R {
    with_service_for_test(None, run)
}

#[cfg(all(test, unix))]
pub(crate) fn with_available_service_for_test<R>(root: &Path, run: impl FnOnce() -> R) -> R {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700)).unwrap();
    let (scopes, _receiver) = mpsc::channel(4);
    let service = Service {
        relay: GhRelay::new(root.join("unused-connection.json")),
        scopes,
        runtime: tokio::runtime::Handle::current(),
        endpoints: Ok(crate::gh_shim_worker_endpoints::Generation::new(root).unwrap()),
        connection_slots: Arc::new(Semaphore::new(64)),
    };
    let scope = ScopeStamp::new(
        subc_protocol::Principal::Reserved {
            module_id: "prefrontal-core".into(),
        },
        "optional-spawn-scope",
        1,
        subc_protocol::scope::ScopeKind::Worker,
    )
    .with_owner_authorized(true)
    .with_attributes(
        subc_protocol::scope::ScopeAttributes::new()
            .with_agent_id(Some("test-worker".into()))
            .with_delegates(true),
    );
    with_service_for_test(Some(service), || {
        crate::bash_background::registry::with_scope_spawn(Some(scope), run)
    })
}

/// Installed for one module connection. Losing that connection invalidates all
/// brokers that captured its channel, including commands surviving a reconnect.
pub(crate) struct ServiceGuard {
    #[cfg(unix)]
    scopes: mpsc::Sender<ScopeQuery>,
}
impl Drop for ServiceGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            let mut service = SERVICE.lock().unwrap_or_else(|p| p.into_inner());
            if service
                .as_ref()
                .is_some_and(|current| current.scopes.same_channel(&self.scopes))
            {
                service.take();
            }
        }
    }
}

pub(crate) fn install(relay: Arc<GhRelay>) -> (ServiceGuard, mpsc::Receiver<ScopeQuery>) {
    let (scopes, rx) = mpsc::channel(32);
    let guard = ServiceGuard {
        #[cfg(unix)]
        scopes: scopes.clone(),
    };
    #[cfg(unix)]
    {
        *SERVICE.lock().unwrap_or_else(|p| p.into_inner()) = Some(Service {
            relay,
            scopes,
            runtime: tokio::runtime::Handle::current(),
            endpoints: crate::gh_shim_worker_endpoints::Generation::new(
                crate::sandbox_spawn::managed_task_temp_root(),
            )
            .map_err(|error| error.to_string()),
            connection_slots: GLOBAL.clone(),
        });
    }
    #[cfg(not(unix))]
    let _ = (relay, scopes);
    (guard, rx)
}

#[cfg(unix)]
fn refusal(code: &str) -> RelayReply {
    RelayReply {
        ok: false,
        data: json!({"refusal_code": code, "stage": "worker_broker", "message": code}),
    }
}

#[cfg(unix)]
struct ReadAudit<'a> {
    identity: &'a crate::gh_shim_ticket::Redeemed,
    read: bool,
    ok: bool,
    sequence: u32,
    emit: &'a (dyn Fn(&str) + Send + Sync),
}
#[cfg(unix)]
impl Drop for ReadAudit<'_> {
    fn drop(&mut self) {
        (self.emit)(&format!(
            "gh_shim worker_read: session={} task={} read={} ok={} sequence={}",
            self.identity.session_id.escape_debug(),
            self.identity.task_id.escape_debug(),
            self.read,
            self.ok,
            self.sequence
        ));
    }
}

#[cfg(unix)]
fn decode(bytes: &[u8]) -> Result<Envelope, RelayReply> {
    if bytes.len() > MAX_FRAME {
        return Err(refusal("frame_too_large"));
    }
    serde_json::from_slice(bytes).map_err(|_| refusal("request_malformed"))
}

#[cfg(unix)]
struct Budget {
    window: Instant,
    requests: u32,
    upstream: u32,
}

#[cfg(unix)]
impl Budget {
    fn take(&mut self, read: bool) -> bool {
        if self.window.elapsed() >= Duration::from_secs(1) {
            self.window = Instant::now();
            self.requests = 0;
        }
        if self.requests >= 30 || (read && self.upstream >= 600) {
            return false;
        }
        self.requests += 1;
        self.upstream += u32::from(read);
        true
    }
}

/// Paths, not bearer bytes, are passed through the environment. The socket is
/// a sibling of the writable task temp, so the jail cannot replace its server.
#[cfg(unix)]
pub(crate) fn start(
    ticket: &crate::gh_shim_ticket::PendingTicket,
    task_id: &str,
    temp: &Path,
    deadline: Instant,
    tasks: crate::bash_background::BgTaskRegistry,
    env: &mut std::collections::HashMap<String, String>,
) -> Result<(), String> {
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let service = command_service().ok_or("worker read broker has no daemon scope service")?;
    if service.scopes.is_closed() {
        return Err("worker read broker daemon scope service is closed".into());
    }
    let name = temp
        .file_name()
        .ok_or("worker temp has no name")?
        .to_string_lossy();
    let generation = service.endpoints.as_ref().map_err(Clone::clone)?;
    let (listener, endpoint) = generation
        .bind(&name)
        .map_err(|e| format!("worker read broker bind failed: {e}"))?;
    let socket = endpoint.path();
    let mut rollback = ProvisionRollback {
        ticket,
        capability_file: None,
        state: None,
        endpoint: Some(endpoint.clone()),
        committed: false,
    };
    ticket.reserve_task(task_id);
    let stamp = crate::bash_background::registry::scope_for_spawn()
        .ok_or("worker read broker requires authenticated spawn scope")?;
    let (capability, cancel) = ticket
        .worker_read_capability(socket.clone(), deadline, &stamp)
        .ok_or("worker read broker grant could not be issued")?;
    if !ticket.attach_worker_read_endpoint(endpoint.clone()) {
        return Err("worker read grant ended during endpoint binding".into());
    }
    let cap_file = temp.join(".gh-read-capability");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&cap_file)
        .map_err(|e| e.to_string())?;
    rollback.capability_file = Some(cap_file.clone());
    file.write_all(capability.as_bytes())
        .map_err(|e| e.to_string())?;
    let state = temp.join(".gh-shim");
    crate::gh_shim::seed_worker_state(&state)?;
    rollback.state = Some(state.clone());
    let listener = {
        let _entered = service.runtime.enter();
        tokio::net::UnixListener::from_std(listener).map_err(|error| error.to_string())?
    };
    env.remove(crate::gh_shim_ticket::GH_SHIM_TICKET_ENV);
    env.insert(SOCKET_ENV.into(), socket.to_string_lossy().into_owned());
    env.insert(
        CAPABILITY_FILE_ENV.into(),
        cap_file.to_string_lossy().into_owned(),
    );
    env.insert(STATE_ENV.into(), state.to_string_lossy().into_owned());
    let runtime = service.runtime.clone();
    runtime.spawn(async move {
        serve(listener, socket.clone(), service, tasks, cancel.subscribe()).await;
        endpoint.remove();
    });
    rollback.committed = true;
    Ok(())
}

#[cfg(unix)]
struct ProvisionRollback<'a> {
    ticket: &'a crate::gh_shim_ticket::PendingTicket,
    capability_file: Option<PathBuf>,
    state: Option<PathBuf>,
    endpoint: Option<Arc<crate::gh_shim_worker_endpoints::Endpoint>>,
    committed: bool,
}
#[cfg(unix)]
impl Drop for ProvisionRollback<'_> {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        self.ticket.abandon_worker_read();
        if let Some(endpoint) = &self.endpoint {
            endpoint.remove();
        }
        if let Some(path) = &self.capability_file {
            let _ = std::fs::remove_file(path);
        }
        if let Some(path) = &self.state {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}

/// Every reason a jailed worker command starts without the read broker is
/// reported through this one warning, so an operator can tell why `gh` reads
/// are unavailable in that shell.
#[cfg(unix)]
pub(crate) fn log_unavailable(task_id: &str, reason: &str) {
    crate::slog_warn!(
        "gh_shim worker_read unavailable: task={} reason={}; command continues without broker authority",
        task_id.escape_debug(),
        reason.escape_debug()
    );
}

/// Broker availability never decides whether a confined shell may start. A
/// failed setup grants nothing and publishes no endpoint paths to that child.
#[cfg(unix)]
pub(crate) fn provision_optional(
    ticket: &crate::gh_shim_ticket::PendingTicket,
    task_id: &str,
    temp: Option<&Path>,
    deadline: Instant,
    tasks: crate::bash_background::BgTaskRegistry,
    env: &mut std::collections::HashMap<String, String>,
    enabled: bool,
) -> bool {
    for key in [
        SOCKET_ENV,
        CAPABILITY_FILE_ENV,
        STATE_ENV,
        crate::gh_shim_ticket::GH_SHIM_TICKET_ENV,
    ] {
        env.remove(key);
    }
    let result = if !enabled {
        Err("worker GitHub shim is disabled".to_string())
    } else {
        temp.ok_or_else(|| "worker read broker has no private task temp".to_string())
            .and_then(|temp| start(ticket, task_id, temp, deadline, tasks, env))
    };
    match result {
        Ok(()) => true,
        Err(reason) => {
            ticket.abandon_worker_read();
            for key in [
                SOCKET_ENV,
                CAPABILITY_FILE_ENV,
                STATE_ENV,
                crate::gh_shim_ticket::GH_SHIM_TICKET_ENV,
            ] {
                env.remove(key);
            }
            log_unavailable(task_id, &reason);
            false
        }
    }
}

#[cfg(unix)]
async fn serve(
    listener: tokio::net::UnixListener,
    socket: PathBuf,
    service: Service,
    tasks: crate::bash_background::BgTaskRegistry,
    mut cancel: watch::Receiver<bool>,
) {
    let local = Arc::new(Semaphore::new(CONNECTIONS));
    let upstream = Arc::new(Semaphore::new(2));
    let budget = Arc::new(Mutex::new(Budget {
        window: Instant::now(),
        requests: 0,
        upstream: 0,
    }));
    let mut connections = tokio::task::JoinSet::new();
    // Bound registry work even for syntactically valid guesses at the bearer.
    // This bucket is checked before allocating a task or authenticating a frame.
    let mut ingress = Budget {
        window: Instant::now(),
        requests: 0,
        upstream: 0,
    };
    loop {
        if *cancel.borrow() {
            break;
        }
        let Some(deadline) = crate::gh_shim_ticket::worker_read_deadline(&socket) else {
            break;
        };
        if Instant::now() >= deadline {
            break;
        }
        tokio::select! {
            biased;
            _ = cancel.changed() => break,
            _ = service.scopes.closed() => break,
            _ = tokio::time::sleep_until(deadline.into()) => {},
            Some(_) = connections.join_next(), if !connections.is_empty() => {},
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { break; };
                if !ingress.take(false) { continue; }
                let (Ok(local), Ok(global)) = (local.clone().try_acquire_owned(), service.connection_slots.clone().try_acquire_owned()) else { continue; };
                let service = service.clone();
                let socket = socket.clone();
                let tasks = tasks.clone();
                let upstream = upstream.clone();
                let budget = budget.clone();
                let mut revoked = cancel.clone();
                connections.spawn(async move {
                    let (_local, _global) = (local, global);
                    if *revoked.borrow() { return; }
                    tokio::select! {
                        biased;
                        _ = revoked.changed() => {},
                        _ = connection(stream, &socket, &service, &tasks, &upstream, &budget) => {},
                    }
                });
            }
        }
    }
    connections.abort_all();
    while connections.join_next().await.is_some() {}
}

#[cfg(unix)]
async fn connection(
    mut stream: tokio::net::UnixStream,
    socket: &Path,
    service: &Service,
    tasks: &crate::bash_background::BgTaskRegistry,
    upstream: &Arc<Semaphore>,
    budget: &Mutex<Budget>,
) {
    let Ok(Ok(bytes)) =
        tokio::time::timeout(FRAME_DEADLINE, read_frame(&mut stream, MAX_FRAME)).await
    else {
        return;
    };
    let Ok(_permit) = upstream.clone().try_acquire_owned() else {
        return;
    };
    let response = tokio::time::timeout(
        CALL_DEADLINE,
        dispatch(
            &bytes,
            socket,
            &service.relay.transport,
            |capability| crate::gh_shim_ticket::redeem_worker_read(capability, socket),
            |task| !tasks.is_task_terminal(task),
            |session| async {
                let (reply, receive) = oneshot::channel();
                service
                    .scopes
                    .try_send(ScopeQuery { session, reply })
                    .map_err(|_| ())?;
                receive.await.map_err(|_| ())
            },
            budget,
        ),
    )
    .await;
    let Ok(response) = response else {
        return;
    };
    let _ = tokio::time::timeout(FRAME_DEADLINE, write_reply(&mut stream, response)).await;
}

#[cfg(unix)]
async fn write_reply<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    mut response: RelayReply,
) -> std::io::Result<()> {
    if response.ok && response.data["isError"] == false {
        if let Some(result) = response.data.get("result") {
            let body = result.get("body").unwrap_or(result);
            let size = match body.as_str() {
                Some(text) => text.len(),
                None => serde_json::to_vec(body)
                    .map_err(std::io::Error::other)?
                    .len(),
            };
            let cap = crate::gh_shim::read_contract::MAX_REPLY_BYTES;
            if size as u64 > cap {
                response = RelayReply {
                    ok: false,
                    data: json!({"refusal_code":"response_too_large","message":format!(
                        "worker read response is {size} bytes; cap is {cap} bytes (1 MiB); narrow the request"
                    )}),
                };
            }
        }
    }
    // Plexus also returns human-readable and structured copies of the result.
    // The worker decoder only consumes these three contract fields; carrying
    // the duplicate bodies can overflow the wire cap below the 1 MiB body cap.
    if response.data["isError"].is_boolean() {
        if let Some(data) = response.data.as_object_mut() {
            data.retain(|key, _| matches!(key.as_str(), "isError" | "result" | "error"));
        }
    }
    let body = json!({"status": if response.ok { "ok" } else { "error" }, "data": response.data});
    let mut bytes = serde_json::to_vec(&body).map_err(std::io::Error::other)?;
    if bytes.len() > MAX_ENCODED_RESPONSE {
        bytes = serde_json::to_vec(&json!({
            "status":"error",
            "data":{"refusal_code":"response_too_large","message":format!(
                "encoded worker read reply is {} bytes; wire cap is {MAX_ENCODED_RESPONSE} bytes (read body cap is {} bytes)",
                bytes.len(), crate::gh_shim::read_contract::MAX_REPLY_BYTES
            )}
        }))
        .map_err(std::io::Error::other)?;
    }
    if bytes.len() <= MAX_RESPONSE {
        return write_frame(writer, &bytes).await;
    }
    // Keep individual frames bounded while allowing the worst-case JSON
    // encoding of a valid body. Both peers enforce the aggregate byte count.
    write_frame(
        writer,
        &serde_json::to_vec(&json!({"status":"chunked","bytes":bytes.len()}))
            .map_err(std::io::Error::other)?,
    )
    .await?;
    for chunk in bytes.chunks(MAX_FRAME) {
        write_frame(writer, chunk).await?;
    }
    Ok(())
}

#[cfg(unix)]
async fn read_reply<R: tokio::io::AsyncRead + Unpin>(reader: &mut R) -> Result<Value, String> {
    let bytes = read_frame(reader, MAX_RESPONSE)
        .await
        .map_err(|error| format!("worker read broker reply failed: {error}"))?;
    let reply: Value = serde_json::from_slice(&bytes)
        .map_err(|_| "worker read broker reply malformed".to_string())?;
    if reply["status"] != "chunked" {
        return Ok(reply);
    }
    let size = reply["bytes"]
        .as_u64()
        .ok_or_else(|| "worker read broker chunk length malformed".to_string())?;
    if size == 0 || size > MAX_ENCODED_RESPONSE as u64 {
        return Err(format!(
            "worker read broker chunked reply is {size} bytes; cap is {MAX_ENCODED_RESPONSE} bytes"
        ));
    }
    let mut bytes = Vec::with_capacity(size as usize);
    while bytes.len() < size as usize {
        let chunk = read_frame(reader, MAX_FRAME.min(size as usize - bytes.len()))
            .await
            .map_err(|error| format!("worker read broker reply chunk failed: {error}"))?;
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| "worker read broker reply malformed".to_string())
}

#[cfg(unix)]
pub(crate) async fn read_frame<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
    cap: usize,
) -> std::io::Result<Vec<u8>> {
    let size = reader.read_u32().await? as usize;
    if size == 0 || size > cap {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("broker frame is {size} bytes; cap is {cap} bytes (size must be nonzero)"),
        ));
    }
    let mut bytes = vec![0; size];
    reader.read_exact(&mut bytes).await?;
    Ok(bytes)
}

#[cfg(unix)]
pub(crate) async fn write_frame<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    bytes: &[u8],
) -> std::io::Result<()> {
    writer.write_u32(bytes.len() as u32).await?;
    writer.write_all(bytes).await
}

#[cfg(unix)]
use crate::gh_shim_relay::RelayTransport;

/// Client discovery never opens user configuration or a general connection file.
/// In worker mode, absent paths are an error rather than a discovery fallback.
#[derive(Clone, Debug)]
pub(crate) enum ClientError {
    #[cfg(not(unix))]
    UnsupportedPlatform(&'static str),
    #[cfg(unix)]
    MissingPath(&'static str),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            #[cfg(not(unix))]
            Self::UnsupportedPlatform(platform) => write!(f, "gh_shim_worker_read_platform_unsupported_{platform}: worker read broker is unavailable on {platform}"),
            #[cfg(unix)]
            Self::MissingPath(key) => write!(f, "worker read broker requires {key}"),
        }
    }
}

pub(crate) struct Client {
    #[cfg(unix)]
    socket: PathBuf,
    #[cfg(unix)]
    capability_file: PathBuf,
}
impl Client {
    pub(crate) fn from_process() -> Result<Self, ClientError> {
        #[cfg(not(unix))]
        return Err(ClientError::UnsupportedPlatform(std::env::consts::OS));
        #[cfg(unix)]
        {
            let path = |key: &'static str| {
                std::env::var_os(key)
                    .map(PathBuf::from)
                    .filter(|p| p.is_absolute())
                    .ok_or(ClientError::MissingPath(key))
            };
            Ok(Self {
                socket: path(SOCKET_ENV)?,
                capability_file: path(CAPABILITY_FILE_ENV)?,
            })
        }
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    #[test]
    fn gh_shim_broker_windows_has_a_typed_platform_refusal() {
        assert!(matches!(
            Client::from_process(),
            Err(ClientError::UnsupportedPlatform("windows"))
        ));
    }
}

pub(crate) fn request(
    client: &Client,
    operation: Operation,
    budget: Duration,
) -> Result<Value, String> {
    #[cfg(not(unix))]
    {
        let _ = (client, operation, budget);
        Err("gh_shim_worker_read_platform_unsupported_windows".into())
    }
    #[cfg(unix)]
    {
        use std::io::Read;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open(&client.capability_file)
            .map_err(|_| "worker read capability file unavailable")?;
        if !file
            .metadata()
            .map_err(|_| "worker read capability metadata unavailable")?
            .is_file()
        {
            return Err("worker read capability must be a regular file".into());
        }
        let mut capability = String::new();
        file.by_ref()
            .take(33)
            .read_to_string(&mut capability)
            .map_err(|_| "worker read capability unavailable")?;
        if capability.len() != 32 || !capability.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("worker read capability malformed".into());
        }
        let bytes = serde_json::to_vec(&Envelope {
            capability,
            operation,
        })
        .map_err(|_| "worker broker request malformed")?;
        if bytes.len() > MAX_FRAME {
            return Err("worker broker request too large".into());
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        runtime.block_on(async {
            tokio::time::timeout(budget.min(CALL_DEADLINE), async {
                let mut stream = tokio::net::UnixStream::connect(&client.socket)
                    .await
                    .map_err(|_| "worker read broker unavailable")?;
                let peer = stream
                    .peer_cred()
                    .map_err(|_| "worker read broker peer unavailable")?;
                if peer.uid() != unsafe { libc::geteuid() } {
                    return Err("worker read broker peer mismatch".to_string());
                }
                write_frame(&mut stream, &bytes)
                    .await
                    .map_err(|_| "worker read broker write failed")?;
                let reply = read_reply(&mut stream).await?;
                match reply["status"].as_str() {
                    Some("ok") => reply
                        .get("data")
                        .cloned()
                        .ok_or_else(|| "worker read broker omitted data".into()),
                    Some("error") => {
                        // Keep the daemon's explanation (for example, which
                        // query keys are allowed) so a refused command can be
                        // corrected without inspecting broker traffic.
                        let code = reply["data"]["refusal_code"].as_str().unwrap_or("refused");
                        Err(match reply["data"]["message"].as_str() {
                            Some(message) if !message.is_empty() => {
                                format!("worker read broker refused: {code}: {message}")
                            }
                            _ => format!("worker read broker refused: {code}"),
                        })
                    }
                    _ => Err("worker read broker reply has unknown status".into()),
                }
            })
            .await
            .map_err(|_| "worker read broker timed out".to_string())?
        })
    }
}

#[cfg(unix)]
async fn dispatch<T, A, L, S, F>(
    bytes: &[u8],
    socket: &Path,
    transport: &T,
    authenticate: A,
    live: L,
    scope: S,
    budget: &Mutex<Budget>,
) -> RelayReply
where
    T: RelayTransport,
    A: Fn(&str) -> Option<crate::gh_shim_ticket::Redeemed>,
    L: Fn(&str) -> bool,
    S: Fn(String) -> F,
    F: std::future::Future<Output = Result<Option<ScopeStamp>, ()>>,
{
    dispatch_recorded(
        bytes,
        socket,
        transport,
        authenticate,
        live,
        scope,
        budget,
        &|line| log::info!("{line}"),
    )
    .await
}

#[cfg(unix)]
async fn dispatch_recorded<T, A, L, S, F>(
    bytes: &[u8],
    socket: &Path,
    transport: &T,
    authenticate: A,
    live: L,
    scope: S,
    budget: &Mutex<Budget>,
    emit: &(dyn Fn(&str) + Send + Sync),
) -> RelayReply
where
    T: RelayTransport,
    A: Fn(&str) -> Option<crate::gh_shim_ticket::Redeemed>,
    L: Fn(&str) -> bool,
    S: Fn(String) -> F,
    F: std::future::Future<Output = Result<Option<ScopeStamp>, ()>>,
{
    let mut envelope = match decode(bytes) {
        Ok(envelope) => envelope,
        Err(reply) => return reply,
    };
    let Some(identity) = authenticate(&envelope.capability) else {
        return refusal("capability_unknown");
    };
    if !live(&identity.task_id) {
        return refusal("command_ended");
    }
    let read = matches!(envelope.operation, Operation::ReadRequest { .. });
    let sequence = {
        let mut limits = budget.lock().unwrap_or_else(|p| p.into_inner());
        if !limits.take(read) {
            return refusal("read_budget_exceeded");
        }
        limits.requests
    };
    // Admission rate limiting bounds log volume, rather than dropping audit
    // records for later reads in the same window. Drop also records cancellation.
    let mut audit = ReadAudit {
        identity: &identity,
        read,
        ok: false,
        sequence,
        emit,
    };
    let reply = async {
    if let Operation::ReadRequest { request } = &mut envelope.operation {
        if let Err(error) = request.canonicalize_worker_path() {
            return RelayReply {ok:false,data:json!({"refusal_code":error.code,"stage":"worker_read_policy","message":error.message})};
        }
    }
    if let Operation::ReadRequest { request } = &envelope.operation {
        if let Err(error) = request.validate_worker_query() {
            return RelayReply { ok:false, data:json!({"refusal_code":error.code,"stage":"worker_read_policy","key":error.key,"allowed_keys":crate::gh_shim::read_contract::WORKER_GET_QUERY_KEYS,"message":error.message}) };
        }
    }
    let Ok(stamp) = scope(identity.session_id.clone()).await else {
        return refusal("scope_unavailable");
    };
    if !stamp.as_ref().is_some_and(|stamp| {
        crate::gh_shim_ticket::worker_read_scope_matches(&envelope.capability, socket, stamp)
    }) {
        return refusal("worker_binding_changed");
    }
    let context = relay::session_context(&identity, stamp.as_ref());
    if !context.ok || context.data["worker"] != true || !transport.opens_scoped_routes() {
        return refusal("agent_identity_required");
    }
    if transport.read_repository(&identity).is_err() {
        return refusal("worker_reads_disabled");
    }
    // Check again after querying the live route table: revocation must also
    // invalidate requests queued while the daemon was binding or closing routes.
    if authenticate(&envelope.capability).is_none() || !live(&identity.task_id) {
        return refusal("command_ended");
    }
    let reply = match envelope.operation {
        Operation::SessionContext => context,
        Operation::ReadRequest { request } => {
            relay::read_validated_request(transport, &identity, stamp.as_ref(), &request)
                .await
                .0
        }
    };
    let final_scope = scope(identity.session_id.clone()).await.ok().flatten();
    if !final_scope.as_ref().is_some_and(|stamp| {
        crate::gh_shim_ticket::worker_read_scope_matches(&envelope.capability, socket, stamp)
    }) {
        return refusal("worker_binding_changed");
    }
    // An upstream read already sent cannot be undone. Suppress its result if
    // the command ended while the call was in flight.
    if authenticate(&envelope.capability).is_none() || !live(&identity.task_id) {
        return refusal("command_ended");
    }
    reply
    }.await;
    audit.ok = reply.ok;
    reply
}

#[cfg(all(test, unix))]
#[path = "gh_shim_worker_broker_tests.rs"]
mod tests;
