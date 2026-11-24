//! Purpose-separated, command-lifetime ingress for jailed GitHub reads.
//!
//! The child controls every byte. Neither the ordinary relay ticket nor a
//! caller-selected identity is accepted here; the general relay is unreachable.

#[cfg(unix)]
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(unix)]
use std::sync::{LazyLock, Mutex};
#[cfg(unix)]
use std::time::{Duration, Instant};

#[cfg(unix)]
use serde_json::json;
#[cfg(all(unix, test))]
use serde_json::Value;
#[cfg(all(unix, test))]
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
#[cfg(unix)]
use tokio::sync::{watch, Semaphore};

#[cfg(unix)]
use crate::gh_shim_relay::{self as relay, RelayReply};
use crate::gh_shim_relay::{GhRelay, ScopeStamp};

#[cfg(all(unix, test))]
use aft_github::gh_shim_worker_broker::test_wire::{
    failure_text, read_reply, write_frame, MAX_RESPONSE,
};
pub use aft_github::gh_shim_worker_broker::*;

#[cfg(unix)]
const FRAME_DEADLINE: Duration = Duration::from_secs(2);
#[cfg(unix)]
const CONNECTIONS: usize = 4;
#[cfg(unix)]
const REQUESTS_PER_SECOND: u32 = 30;
#[cfg(unix)]
const READS_PER_COMMAND: u32 = 600;
#[cfg(unix)]
static GLOBAL: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(64)));

#[doc(hidden)]
pub struct ScopeQuery {
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

#[cfg(all(unix, any(test, feature = "test-support")))]
thread_local! {
    static TEST_SERVICE: std::cell::RefCell<Option<Option<Service>>> = const { std::cell::RefCell::new(None) };
}

#[cfg(unix)]
fn command_service() -> Option<Service> {
    #[cfg(any(test, feature = "test-support"))]
    if let Some(service) = TEST_SERVICE.with(|slot| slot.borrow().clone()) {
        return service;
    }
    SERVICE.lock().unwrap_or_else(|p| p.into_inner()).clone()
}

#[cfg(all(unix, any(test, feature = "test-support")))]
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

#[cfg(all(unix, any(test, feature = "test-support")))]
pub(crate) fn without_service_for_test<R>(run: impl FnOnce() -> R) -> R {
    with_service_for_test(None, run)
}

#[cfg(all(unix, any(test, feature = "test-support")))]
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
            module_id: "prefrontal".into(),
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
#[doc(hidden)]
pub struct ServiceGuard {
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

#[doc(hidden)]
pub fn install(relay: Arc<GhRelay>) -> (ServiceGuard, mpsc::Receiver<ScopeQuery>) {
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
struct Budget {
    window: Instant,
    requests: u32,
    upstream: u32,
}

#[cfg(unix)]
impl Budget {
    fn take(&mut self, read: bool) -> Result<(), &'static str> {
        if self.window.elapsed() >= Duration::from_secs(1) {
            self.window = Instant::now();
            self.requests = 0;
        }
        if read && self.upstream >= READS_PER_COMMAND {
            return Err("command_budget_exceeded");
        }
        if self.requests >= REQUESTS_PER_SECOND {
            return Err("request_rate_exceeded");
        }
        self.requests += 1;
        self.upstream += u32::from(read);
        Ok(())
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
    env.remove(crate::gh_shim_paths::GH_SHIM_TICKET_ENV);
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
        crate::gh_shim_paths::GH_SHIM_TICKET_ENV,
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
                crate::gh_shim_paths::GH_SHIM_TICKET_ENV,
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
                let Ok((mut stream, _)) = accepted else { break; };
                let admission = ingress.take(false).and_then(|()| {
                    let local = local.clone().try_acquire_owned().map_err(|_| "broker_busy")?;
                    let global = service.connection_slots.clone().try_acquire_owned().map_err(|_| "broker_busy")?;
                    Ok((local, global))
                });
                let (local, global) = match admission {
                    Ok(permits) => permits,
                    Err(code) => {
                        // Do not allocate an unbounded rejection task under load.
                        let _ = tokio::time::timeout(FRAME_DEADLINE, write_reply(&mut stream, refusal(code))).await;
                        continue;
                    }
                };
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
    let response = tokio::time::timeout(CALL_DEADLINE, async {
        // The admitted connection slots bound this queue. A four-process burst
        // must not lose its third and fourth requests to a silent EOF.
        let Ok(_permit) = upstream.acquire().await else {
            return refusal("broker_unavailable");
        };
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
        )
        .await
    })
    .await;
    let response = response.unwrap_or_else(|_| refusal("broker_timeout"));
    let _ = tokio::time::timeout(FRAME_DEADLINE, write_reply(&mut stream, response)).await;
}

#[cfg(unix)]
use crate::gh_shim_relay::RelayTransport;

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
        if let Err(code) = limits.take(read) {
            return refusal(code);
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
