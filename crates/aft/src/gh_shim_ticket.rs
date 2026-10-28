//! Per-command tickets that let a `gh` shim child speak through the daemon.
//!
//! An agent's children run without any subc credentials, so the `gh` shim
//! cannot prove on its own which agent session launched it. The daemon issues
//! every spawned command an unguessable ticket, hands it to the child as
//! [`GH_SHIM_TICKET_ENV`], and remembers which session and task it belongs to.
//! When the shim asks the daemon to relay a bot write, the ticket is the only
//! thing that selects the session; a session id typed on a command line or
//! placed in the environment carries no authority.
//!
//! Tickets live in this process's memory and nowhere else. They are never
//! written to task JSON, aft.db or the log, they die when their task reaches a
//! terminal state, and a daemon restart forgets all of them, so a task replayed
//! after a restart has no ticket and its shim calls are refused.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::sync::{LazyLock, Mutex};

/// Environment variable that carries the ticket into a spawned command.
pub const GH_SHIM_TICKET_ENV: &str = "AFT_GH_SHIM_TICKET";
/// Restriction hint added only by worker launch planning. It cannot grant identity.
pub const GH_SHIM_WORKER_ENV: &str = "AFT_GH_SHIM_WORKER";

const TICKET_BYTES: usize = 16;

/// The session and command a redeemed ticket was issued for.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Redeemed {
    pub session_id: String,
    /// Background task id, or the id of the tool call that ran `gh` directly.
    /// Empty while a background task is still being spawned.
    pub task_id: String,
    /// Project root recorded when the daemon launched the command. It is used
    /// to capture read_repository at mint and to label outbound routes, not to
    /// re-derive a repository from editable config during a later read.
    pub project_root: String,
    /// Repository captured from the primary checkout when this ticket was
    /// minted. Later worktree config edits cannot retarget an existing ticket.
    pub read_repository: Option<String>,
    pub(crate) launch: Option<crate::gh_shim_speech::model::LaunchAuthority>,
    pub(crate) lifecycle_scope: Option<subc_protocol::scope::ScopeStamp>,
}

enum Owner {
    Task(String),
    Call(String),
}

impl Owner {
    fn id(&self) -> &str {
        match self {
            Self::Task(id) | Self::Call(id) => id,
        }
    }
}

struct Entry {
    ticket: [u8; TICKET_BYTES],
    session_id: String,
    owner: Owner,
    project_root: String,
    read_repository: Option<String>,
    launch: Option<crate::gh_shim_speech::model::LaunchAuthority>,
    lifecycle_scope: Option<subc_protocol::scope::ScopeStamp>,
    speech: Option<Arc<SpeechTicket>>,
    #[cfg(unix)]
    worker_read: Option<WorkerReadGrant>,
}

pub(crate) struct SpeechTicket {
    pub(crate) prepared: crate::gh_shim_speech::service::PreparedLaunch,
    pub(crate) incarnation: String,
    pub(crate) stored: AtomicBool,
    pub(crate) background: AtomicBool,
    pub(crate) active: AtomicBool,
    pub(crate) failure: Mutex<Option<crate::gh_shim_speech::Refusal>>,
}

thread_local! {static SPAWN_TICKET:std::cell::RefCell<Option<[u8;TICKET_BYTES]>>=const{std::cell::RefCell::new(None)};}

pub(crate) struct SpawnTicketGuard(Option<[u8; TICKET_BYTES]>);
impl Drop for SpawnTicketGuard {
    fn drop(&mut self) {
        SPAWN_TICKET.with(|slot| *slot.borrow_mut() = self.0.take());
    }
}
pub(crate) fn spawn_ticket_guard(ticket: Option<&str>) -> SpawnTicketGuard {
    SpawnTicketGuard(SPAWN_TICKET.with(|slot| slot.replace(ticket.and_then(decode))))
}

pub(crate) fn capture_spawn(metadata: &crate::bash_background::persistence::PersistedTask) {
    let Some((speech, _)) = spawn_speech(&metadata.task_id) else {
        return;
    };
    capture_record(&speech, &metadata.session_id, &metadata.task_id);
}

pub(crate) fn pty_seal_required() -> bool {
    let raw = SPAWN_TICKET.with(|slot| *slot.borrow());
    let Some(raw) = raw else { return false };
    registry()
        .iter()
        .find(|entry| constant_time_eq(&entry.ticket, &raw))
        .and_then(|entry| entry.speech.as_ref())
        .is_some_and(|speech| {
            speech.stored.load(Ordering::SeqCst)
                && speech.prepared.denial.is_none()
                && speech.prepared.policy.is_some()
        })
}

pub(crate) fn finish_pty_seal_or_revoke(
    task_id: &str,
) -> Result<(), crate::gh_shim_speech::Refusal> {
    use crate::gh_shim_speech::{Code, Refusal, ScopeRole};
    let Some((speech, _)) = spawn_speech(task_id) else {
        return Err(Refusal::new(
            Code::AuthorityUnavailable,
            "task_unverifiable",
        ));
    };
    let stored = speech
        .prepared
        .context
        .service
        .store
        .get(&speech.incarnation);
    let stored_error = stored.as_ref().err().cloned();
    let failure = if stored_error
        .as_ref()
        .is_some_and(|error| error.reason == "revoked" || error.code == Code::ScopeEnded)
    {
        stored_error
    } else {
        speech
            .failure
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
            .or(stored_error)
    };
    if let Some(error) = &failure {
        // A speech-bookkeeping failure may downgrade a launch. A task cancel or
        // a definitive end of its work scope must still stop the command itself.
        let lifecycle_ended = error.code == Code::ScopeEnded
            && speech
                .prepared
                .context
                .seed
                .launch
                .as_ref()
                .is_ok_and(|launch| {
                    let ended = if error.role == Some(ScopeRole::Run) {
                        launch.run.as_ref()
                    } else {
                        Some(&launch.authority)
                    };
                    ended
                        .zip(speech.prepared.context.seed.lifecycle.as_ref())
                        .is_some_and(|(ended, lifecycle)| {
                            crate::db::access_grants::ScopeKey::from_stamp(ended)
                                == crate::db::access_grants::ScopeKey::from_stamp(lifecycle)
                        })
                });
        if error.reason == "revoked" || lifecycle_ended {
            return Err(error.clone());
        }
    }
    if failure.is_none()
        && speech.active.load(Ordering::SeqCst)
        && stored.is_ok_and(|stored| stored.record.seal.is_some())
    {
        return Ok(());
    }
    speech
        .prepared
        .context
        .service
        .revoke(&speech.incarnation, "execution_unsealed")?;
    speech.active.store(false, Ordering::SeqCst);
    *speech
        .failure
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = Some(
        failure
            .filter(|error| error.code == Code::ScopeEnded)
            .unwrap_or_else(|| Refusal::new(Code::AuthorityUnavailable, "revoked")),
    );
    Ok(())
}

fn capture_record(speech: &SpeechTicket, session: &str, task: &str) {
    if !speech.prepared.provisioned {
        return;
    }
    let launch = match &speech.prepared.context.seed.launch {
        Ok(launch) => launch.clone(),
        Err(error) => {
            *speech
                .failure
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = Some(error.clone());
            return;
        }
    };
    let record = crate::gh_shim_speech::model::AuthorityRecord {
        incarnation: speech.incarnation.clone(),
        minting_run: crate::gh_shim_speech::service::run_nonce()
            .unwrap_or_default()
            .into(),
        harness: speech.prepared.context.seed.harness.clone(),
        session_id: session.to_string(),
        task_id: task.to_string(),
        project_root: speech.prepared.context.seed.project_root.clone(),
        launch,
        policy: speech.prepared.policy.clone(),
        bindings: speech.prepared.bindings.clone(),
        seal: None,
        ineligible_reason: speech
            .prepared
            .denial
            .as_ref()
            .map(|error| error.reason.clone()),
    };
    match speech.prepared.context.service.store.capture(&record) {
        Ok(()) => {
            speech.stored.store(true, Ordering::SeqCst);
        }
        Err(error) => {
            *speech
                .failure
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = Some(error)
        }
    }
}

pub(crate) fn seal_spawn(metadata: &crate::bash_background::persistence::PersistedTask) {
    let Some((speech, raw)) = spawn_speech(&metadata.task_id) else {
        return;
    };
    if !speech.stored.load(Ordering::SeqCst) {
        return;
    }
    #[cfg(test)]
    if let Some(error) = speech.prepared.context.service.test_seal_failure() {
        *speech
            .failure
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(error);
        return;
    }
    #[cfg(unix)]
    if metadata.remote.is_some() {
        *speech
            .failure
            .lock()
            .unwrap_or_else(|error| error.into_inner()) =
            Some(crate::gh_shim_speech::Refusal::new(
                crate::gh_shim_speech::Code::AuthorityUnavailable,
                "guard_unsupported",
            ));
        return;
    }
    let seal = metadata
        .child_pid
        .zip(metadata.child_start_time_ms)
        .and_then(|(pid, start)| {
            if crate::root_cache::process_start_time_ms(pid) != Some(start) {
                return None;
            }
            Some(crate::gh_shim_speech::model::ExecutionSeal {
                child_pid: pid,
                child_start_time_ms: start,
                birth_identity: crate::fs_lock::speech_process_identity(pid)?,
            })
        });
    let result = seal
        .ok_or_else(|| {
            crate::gh_shim_speech::Refusal::new(
                crate::gh_shim_speech::Code::AuthorityUnavailable,
                "task_unverifiable",
            )
        })
        .and_then(|seal| {
            speech
                .prepared
                .context
                .service
                .store
                .seal(&speech.incarnation, &seal)
        });
    if let Err(error) = result {
        *speech
            .failure
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(error);
        return;
    }
    if speech.background.load(Ordering::SeqCst) {
        activate_speech(&speech, &raw);
    }
}

fn activate_speech(speech: &SpeechTicket, raw: &[u8; TICKET_BYTES]) {
    if speech.active.load(Ordering::SeqCst)
        || speech
            .failure
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .is_some()
    {
        return;
    }
    let digest = blake3::derive_key("aft-gh-speech-ticket-v1", raw);
    match speech.prepared.context.service.store.activate(
        &speech.incarnation,
        &digest,
        crate::gh_shim_speech::service::run_nonce().unwrap_or_default(),
    ) {
        Ok(()) => {
            speech.active.store(true, Ordering::SeqCst);
        }
        Err(error) => {
            *speech
                .failure
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = Some(error)
        }
    }
}

pub(crate) fn mark_spawn_background(background: bool) {
    let raw = SPAWN_TICKET.with(|slot| *slot.borrow());
    if let Some(raw) = raw {
        if let Some(speech) = registry()
            .iter()
            .find(|entry| constant_time_eq(&entry.ticket, &raw))
            .and_then(|entry| entry.speech.clone())
        {
            speech.background.store(background, Ordering::SeqCst);
        }
    }
}

pub(crate) fn promote_speech_task(session: &str, task: &str) {
    let entries: Vec<_> = registry()
        .iter()
        .filter(|entry| {
            entry.session_id == session && matches!(&entry.owner,Owner::Task(id)if id==task)
        })
        .filter_map(|entry| entry.speech.clone().map(|speech| (speech, entry.ticket)))
        .collect();
    for (speech, raw) in entries {
        speech.background.store(true, Ordering::SeqCst);
        activate_speech(&speech, &raw);
    }
}

fn spawn_speech(task_id: &str) -> Option<(Arc<SpeechTicket>, [u8; TICKET_BYTES])> {
    let raw = SPAWN_TICKET.with(|slot| *slot.borrow())?;
    let mut entries = registry();
    let entry = entries
        .iter_mut()
        .find(|entry| constant_time_eq(&entry.ticket, &raw))?;
    entry.owner = Owner::Task(task_id.to_string());
    Some((entry.speech.clone()?, raw))
}

pub(crate) fn speech_digest(text: &str) -> Option<[u8; 32]> {
    Some(blake3::derive_key(
        "aft-gh-speech-ticket-v1",
        &decode(text)?,
    ))
}

pub(crate) fn speech_for_ticket(text: &str) -> Option<Arc<SpeechTicket>> {
    let raw = decode(text)?;
    registry()
        .iter()
        .find(|entry| constant_time_eq(&entry.ticket, &raw))
        .and_then(|entry| entry.speech.clone())
}

/// The worker-read capability is separate from the general relay ticket. Dropping
/// the registry entry tears down its endpoint and every outstanding exchange.
#[cfg(unix)]
struct WorkerReadGrant {
    capability: [u8; TICKET_BYTES],
    cancel: tokio::sync::watch::Sender<bool>,
    socket: std::path::PathBuf,
    deadline: std::time::Instant,
    binding: serde_json::Value,
    endpoint: Option<std::sync::Arc<crate::gh_shim_worker_endpoints::Endpoint>>,
}

#[cfg(unix)]
impl Drop for WorkerReadGrant {
    fn drop(&mut self) {
        self.cancel.send_replace(true);
        if let Some(endpoint) = &self.endpoint {
            endpoint.remove();
        }
    }
}

static REGISTRY: LazyLock<Mutex<Vec<Entry>>> = LazyLock::new(|| Mutex::new(Vec::new()));

fn registry() -> std::sync::MutexGuard<'static, Vec<Entry>> {
    REGISTRY.lock().unwrap_or_else(|poison| poison.into_inner())
}

/// Whether a session may hold a ticket. Requests without an agent session use
/// the shared default namespace, which is not an agent and cannot speak.
fn session_can_hold_ticket(session_id: &str) -> bool {
    !session_id.trim().is_empty() && session_id != crate::protocol::DEFAULT_SESSION_ID
}

fn encode(ticket: &[u8; TICKET_BYTES]) -> String {
    ticket.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn decode(text: &str) -> Option<[u8; TICKET_BYTES]> {
    let text = text.trim();
    if text.len() != TICKET_BYTES * 2 || !text.is_ascii() {
        return None;
    }
    let mut ticket = [0_u8; TICKET_BYTES];
    for (index, slot) in ticket.iter_mut().enumerate() {
        *slot = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(ticket)
}

/// Compare two tickets without an early exit, so the time taken does not
/// reveal how many leading bytes of a guess were right.
fn constant_time_eq(left: &[u8; TICKET_BYTES], right: &[u8; TICKET_BYTES]) -> bool {
    let mut difference = 0_u8;
    for (a, b) in left.iter().zip(right.iter()) {
        difference |= a ^ b;
    }
    difference == 0
}

fn insert(session_id: &str, owner: Owner, project_root: &str) -> Option<String> {
    if !session_can_hold_ticket(session_id) {
        return None;
    }
    let mut ticket = [0_u8; TICKET_BYTES];
    // An operating-system random source failure leaves the command without a
    // ticket, which makes its governed writes refuse rather than guessable.
    getrandom::fill(&mut ticket).ok()?;
    let encoded = encode(&ticket);
    let context = crate::gh_shim_speech::service::current();
    if context
        .as_ref()
        .is_some_and(|context| context.seed.session != session_id)
    {
        return None;
    }
    let lifecycle = context
        .as_ref()
        .and_then(|context| context.seed.lifecycle.clone())
        .or_else(crate::bash_background::registry::scope_for_spawn);
    let launch = context
        .as_ref()
        .and_then(|context| context.seed.launch.as_ref().ok().cloned())
        .or_else(|| {
            lifecycle.clone().and_then(|stamp| {
                crate::gh_shim_speech::model::LaunchAuthority::classic(stamp, None).ok()
            })
        });
    let worker = lifecycle
        .as_ref()
        .is_some_and(|scope| scope.kind == subc_protocol::scope::ScopeKind::Worker);
    let prepared = context.filter(|_| !worker).map(|context| context.prepare());
    let call_id = match &owner {
        Owner::Call(id) => Some(id.clone()),
        Owner::Task(_) => None,
    };
    let speech = prepared.map(|prepared| {
        Arc::new(SpeechTicket {
            failure: Mutex::new(prepared.denial.clone()),
            prepared,
            incarnation: blake3::derive_key("aft-gh-task-incarnation-v1", &ticket)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
            stored: AtomicBool::new(false),
            background: AtomicBool::new(false),
            active: AtomicBool::new(false),
        })
    });
    if let (Some(call), Some(speech)) = (&call_id, &speech) {
        capture_record(
            speech,
            session_id,
            &format!("call:{call}:{}", speech.incarnation),
        );
    }
    let read_repository =
        crate::gh_shim::ticket_repository_snapshot(std::path::Path::new(project_root));
    registry().push(Entry {
        ticket,
        session_id: session_id.to_string(),
        owner,
        project_root: project_root.to_string(),
        read_repository,
        launch,
        lifecycle_scope: lifecycle,
        speech,
        #[cfg(unix)]
        worker_read: None,
    });
    Some(encoded)
}

/// Look a ticket up. Every registered ticket is compared in full, so the
/// lookup time depends on the number of live tickets, not on the guess.
pub fn redeem(ticket: &str) -> Option<Redeemed> {
    let candidate = decode(ticket)?;
    let entries = registry();
    let mut found = None;
    for entry in entries.iter() {
        if constant_time_eq(&entry.ticket, &candidate) {
            found = Some(Redeemed {
                session_id: entry.session_id.clone(),
                task_id: entry.owner.id().to_string(),
                project_root: entry.project_root.clone(),
                read_repository: entry.read_repository.clone(),
                launch: entry.launch.clone(),
                lifecycle_scope: entry.lifecycle_scope.clone(),
            });
        }
    }
    found
}

/// Forget every ticket issued for a task. Called whenever the task's metadata
/// is written in a terminal state and when its bundle is deleted.
pub fn revoke_task(task_id: &str) {
    if task_id.is_empty() {
        return;
    }
    let mut entries = registry();
    let revocations: Vec<_> = entries
        .iter()
        .filter(|entry| matches!(&entry.owner,Owner::Task(id)if id==task_id))
        .filter_map(|entry| entry.speech.clone())
        .collect();
    entries.retain(|entry| !matches!(&entry.owner, Owner::Task(id) if id == task_id));
    drop(entries);
    for speech in revocations {
        queue_terminal_marker(speech);
    }
}

fn revoke(ticket: &str) {
    let Some(candidate) = decode(ticket) else {
        return;
    };
    let mut entries = registry();
    let speech = entries
        .iter()
        .find(|entry| constant_time_eq(&entry.ticket, &candidate))
        .and_then(|entry| entry.speech.clone());
    entries.retain(|entry| !constant_time_eq(&entry.ticket, &candidate));
    drop(entries);
    if let Some(speech) = speech {
        queue_terminal_marker(speech);
    }
}

fn queue_terminal_marker(speech: Arc<SpeechTicket>) {
    if speech.stored.load(Ordering::SeqCst) {
        let work = speech.prepared.context.service.track_finalization();
        // Task-file publication can hold task state, so durable SQL runs afterward.
        // Once removed from memory, a ticket minted by this daemon cannot use the
        // database-backed redemption path reserved for tickets from an earlier daemon.
        std::thread::spawn(move || {
            let _work = work;
            let _ = speech
                .prepared
                .context
                .service
                .revoke(&speech.incarnation, "terminal");
        });
    }
}

/// Number of live tickets; lets tests prove a ticket was dropped.
#[cfg(all(test, unix))]
pub(crate) fn live_count_for_task(task_id: &str) -> usize {
    registry()
        .iter()
        .filter(|entry| matches!(&entry.owner, Owner::Task(id) if id == task_id))
        .count()
}

/// A ticket for a background task that has not been spawned yet, so its task
/// id is still unknown. Dropping it without [`PendingTicket::bind_task`]
/// revokes it, which covers every early return of a failed spawn.
pub struct PendingTicket {
    ticket: Option<String>,
}

impl PendingTicket {
    /// Issue a ticket for `session_id`, or none for a session that is absent
    /// or the shared default namespace.
    pub fn issue(session_id: &str, project_root: &str) -> Self {
        Self {
            ticket: insert(session_id, Owner::Task(String::new()), project_root),
        }
    }

    /// A caller-selected command workdir is not a repository binding. Missing
    /// trusted project context leaves reads unbound rather than inferring it.
    pub(crate) fn issue_for_project(
        session_id: &str,
        project_root: Option<&std::path::Path>,
    ) -> Self {
        Self::issue(
            session_id,
            &project_root
                .map(|root| root.display().to_string())
                .unwrap_or_default(),
        )
    }

    pub fn value(&self) -> Option<&str> {
        self.ticket.as_deref()
    }

    /// Reserve the task identity before exec, while retaining drop-on-error
    /// revocation until the spawning code commits the ticket with bind_task.
    #[cfg(unix)]
    pub(crate) fn reserve_task(&self, task_id: &str) {
        if let Some(candidate) = self.ticket.as_deref().and_then(decode) {
            for entry in registry().iter_mut() {
                if constant_time_eq(&entry.ticket, &candidate) {
                    entry.owner = Owner::Task(task_id.to_string());
                }
            }
        }
    }

    /// Called only after resolving a native worker-jail plan with the shim
    /// enabled. Host synchronous calls have no entry point to mint this grant.
    #[cfg(unix)]
    pub(crate) fn worker_read_capability(
        &self,
        socket: std::path::PathBuf,
        deadline: std::time::Instant,
        binding: &crate::gh_shim_relay::ScopeStamp,
    ) -> Option<(String, tokio::sync::watch::Sender<bool>)> {
        if !crate::gh_shim_relay::worker_scope(binding) {
            return None;
        }
        let candidate = decode(self.ticket.as_deref()?)?;
        let mut entries = registry();
        let entry = entries
            .iter_mut()
            .find(|entry| constant_time_eq(&entry.ticket, &candidate))?;
        if !matches!(&entry.owner, Owner::Task(id) if !id.is_empty()) || entry.worker_read.is_some()
        {
            return None;
        }
        let mut capability = [0; TICKET_BYTES];
        getrandom::fill(&mut capability).ok()?;
        let (cancel, _) = tokio::sync::watch::channel(false);
        entry.worker_read = Some(WorkerReadGrant {
            capability,
            cancel: cancel.clone(),
            socket,
            deadline,
            binding: serde_json::to_value(binding).ok()?,
            endpoint: None,
        });
        Some((encode(&capability), cancel))
    }

    #[cfg(unix)]
    pub(crate) fn attach_worker_read_endpoint(
        &self,
        endpoint: std::sync::Arc<crate::gh_shim_worker_endpoints::Endpoint>,
    ) -> bool {
        let Some(candidate) = self.ticket.as_deref().and_then(decode) else {
            return false;
        };
        let mut entries = registry();
        let Some(grant) = entries
            .iter_mut()
            .find(|entry| constant_time_eq(&entry.ticket, &candidate))
            .and_then(|entry| entry.worker_read.as_mut())
        else {
            return false;
        };
        if grant.socket != endpoint.path() {
            return false;
        }
        grant.endpoint = Some(endpoint);
        true
    }

    /// Abandon only optional read authority after a failed setup. The pending
    /// command ticket still follows its normal bind/drop/terminal lifecycle.
    #[cfg(unix)]
    pub(crate) fn abandon_worker_read(&self) {
        let Some(candidate) = self.ticket.as_deref().and_then(decode) else {
            return;
        };
        let mut entries = registry();
        if let Some(entry) = entries
            .iter_mut()
            .find(|entry| constant_time_eq(&entry.ticket, &candidate))
        {
            entry.worker_read.take();
        }
    }

    /// Attach the spawned task's id so the task's terminal transition revokes
    /// the ticket. The caller must then check whether the task already ended
    /// (see `revoke_task`), because a very short command can reach its
    /// terminal state before this runs.
    pub fn bind_task(mut self, task_id: &str) {
        let Some(ticket) = self.ticket.take() else {
            return;
        };
        let Some(candidate) = decode(&ticket) else {
            return;
        };
        for entry in registry().iter_mut() {
            if constant_time_eq(&entry.ticket, &candidate) {
                entry.owner = Owner::Task(task_id.to_string());
            }
        }
    }
}

/// Redeem only at the endpoint that minted this purpose-specific capability.
/// A general ticket, another command's endpoint, and expired grants all fail.
#[cfg(unix)]
pub(crate) fn redeem_worker_read(capability: &str, socket: &std::path::Path) -> Option<Redeemed> {
    let candidate = decode(capability)?;
    let entries = registry();
    entries.iter().find_map(|entry| {
        let grant = entry.worker_read.as_ref()?;
        if !constant_time_eq(&grant.capability, &candidate)
            || grant.socket != socket
            || *grant.cancel.borrow()
            || std::time::Instant::now() >= grant.deadline
        {
            return None;
        }
        Some(Redeemed {
            session_id: entry.session_id.clone(),
            task_id: entry.owner.id().to_string(),
            project_root: entry.project_root.clone(),
            read_repository: entry.read_repository.clone(),
            launch: None,
            lifecycle_scope: entry.lifecycle_scope.clone(),
        })
    })
}

#[cfg(unix)]
pub(crate) fn worker_read_scope_matches(
    capability: &str,
    socket: &std::path::Path,
    stamp: &crate::gh_shim_relay::ScopeStamp,
) -> bool {
    let Some(candidate) = decode(capability) else {
        return false;
    };
    let Ok(binding) = serde_json::to_value(stamp) else {
        return false;
    };
    registry().iter().any(|entry| {
        entry.worker_read.as_ref().is_some_and(|grant| {
            constant_time_eq(&grant.capability, &candidate)
                && grant.socket == socket
                && grant.binding == binding
        })
    })
}

#[cfg(unix)]
pub(crate) fn renew_worker_read_deadline(task_id: &str, deadline: std::time::Instant) {
    for entry in registry().iter_mut() {
        if matches!(&entry.owner, Owner::Task(id) if id == task_id) {
            if let Some(grant) = &mut entry.worker_read {
                grant.deadline = grant.deadline.max(deadline);
            }
        }
    }
}

#[cfg(unix)]
pub(crate) fn worker_read_deadline(socket: &std::path::Path) -> Option<std::time::Instant> {
    registry()
        .iter()
        .filter_map(|entry| entry.worker_read.as_ref())
        .find(|grant| grant.socket == socket)
        .map(|grant| grant.deadline)
}

impl Drop for PendingTicket {
    fn drop(&mut self) {
        if let Some(ticket) = self.ticket.take() {
            revoke(&ticket);
        }
    }
}

/// A ticket that lives exactly as long as one synchronous `gh` run made by
/// the daemon itself, such as a comment written through the read/edit tools.
pub struct ScopedTicket {
    ticket: Option<String>,
}

impl ScopedTicket {
    pub fn issue(session_id: &str, call_id: &str, project_root: &str) -> Self {
        Self {
            ticket: insert(session_id, Owner::Call(call_id.to_string()), project_root),
        }
    }

    pub fn value(&self) -> Option<&str> {
        self.ticket.as_deref()
    }
}

impl Drop for ScopedTicket {
    fn drop(&mut self) {
        if let Some(ticket) = self.ticket.take() {
            revoke(&ticket);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticket_captures_the_authenticated_launch_scope_before_routes_change() {
        let scope = subc_protocol::scope::ScopeStamp::new(
            subc_protocol::Principal::Reserved {
                module_id: "prefrontal-core".into(),
            },
            "original-scope",
            7,
            subc_protocol::scope::ScopeKind::Head,
        )
        .with_attributes(
            subc_protocol::scope::ScopeAttributes::new()
                .with_agent_id(Some("agent-a".into()))
                .with_delegates(true),
        )
        .with_owner_authorized(true);
        let pending =
            crate::bash_background::registry::with_scope_spawn(Some(scope.clone()), || {
                PendingTicket::issue("launch-pin-session", "/does-not-exist")
            });
        let redeemed = redeem(pending.value().unwrap()).unwrap();
        assert_eq!(
            redeemed.launch.as_ref().map(|launch| &launch.authority),
            Some(&scope)
        );
        assert_eq!(redeemed.lifecycle_scope.as_ref(), Some(&scope));
    }

    #[test]
    fn tickets_are_128_bit_hex_and_unique() {
        let first = ScopedTicket::issue("ses-ticket-unique", "call-a", "/p");
        let second = ScopedTicket::issue("ses-ticket-unique", "call-b", "/p");
        let first_value = first.value().unwrap();
        assert_eq!(first_value.len(), 32);
        assert!(first_value.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(first_value, second.value().unwrap());
    }

    #[test]
    fn default_and_empty_sessions_get_no_ticket() {
        assert!(
            PendingTicket::issue(crate::protocol::DEFAULT_SESSION_ID, "/p")
                .value()
                .is_none()
        );
        assert!(PendingTicket::issue("", "/p").value().is_none());
        assert!(ScopedTicket::issue("  ", "call", "/p").value().is_none());
    }

    #[test]
    fn redeem_returns_the_issuing_session_and_task() {
        let pending = PendingTicket::issue("ses-redeem", "/p");
        let value = pending.value().unwrap().to_string();
        assert_eq!(
            redeem(&value),
            Some(Redeemed {
                session_id: "ses-redeem".to_string(),
                task_id: String::new(),
                project_root: "/p".to_string(),
                read_repository: None,
                launch: None,
                lifecycle_scope: None,
            })
        );
        pending.bind_task("task-redeem");
        assert_eq!(redeem(&value).unwrap().task_id, "task-redeem");
        revoke_task("task-redeem");
        assert_eq!(redeem(&value), None);
    }

    #[test]
    fn fabricated_and_malformed_tickets_do_not_redeem() {
        let _live = ScopedTicket::issue("ses-fabricated", "call", "/p");
        assert_eq!(redeem("00000000000000000000000000000000"), None);
        assert_eq!(redeem("ses-fabricated"), None);
        assert_eq!(redeem(""), None);
        assert_eq!(redeem("zz000000000000000000000000000000"), None);
    }

    #[test]
    fn dropping_an_unbound_pending_ticket_revokes_it() {
        let pending = PendingTicket::issue("ses-drop", "/p");
        let value = pending.value().unwrap().to_string();
        drop(pending);
        assert_eq!(redeem(&value), None);
    }

    #[test]
    fn scoped_ticket_dies_with_its_scope() {
        let value = {
            let scoped = ScopedTicket::issue("ses-scoped", "call-scoped", "/p");
            let value = scoped.value().unwrap().to_string();
            assert!(redeem(&value).is_some());
            value
        };
        assert_eq!(redeem(&value), None);
    }

    #[test]
    fn revoking_a_background_task_does_not_revoke_a_same_named_scoped_call() {
        let scoped = ScopedTicket::issue("ses-collision", "task-call-collision", "/p");
        let scoped_value = scoped.value().unwrap().to_string();
        let pending = PendingTicket::issue("ses-collision", "/p");
        let task_value = pending.value().unwrap().to_string();
        pending.bind_task("task-call-collision");

        revoke_task("task-call-collision");

        assert_eq!(redeem(&task_value), None);
        assert_eq!(
            redeem(&scoped_value).unwrap().task_id,
            "task-call-collision"
        );
        drop(scoped);
        assert_eq!(redeem(&scoped_value), None);
    }
}

pub(crate) fn redeem_speech_cold(
    ticket: &str,
    service: &crate::gh_shim_speech::service::Service,
) -> Result<Redeemed, crate::gh_shim_speech::Refusal> {
    use crate::gh_shim_speech::{Code, Refusal};
    let invalid = || Refusal::new(Code::AuthorityUnavailable, "task_unverifiable");
    let digest = speech_digest(ticket).ok_or_else(invalid)?;
    let stored = service.store.find_digest(
        &digest,
        crate::gh_shim_speech::service::run_nonce().unwrap_or_default(),
    )?;
    let seal = stored.record.seal.as_ref().ok_or_else(invalid)?;
    if crate::root_cache::process_start_time_ms(seal.child_pid) != Some(seal.child_start_time_ms)
        || crate::fs_lock::speech_process_identity(seal.child_pid).as_deref()
            != Some(seal.birth_identity.as_str())
    {
        return Err(invalid());
    }
    let conn = service.store.connect()?;
    let row = crate::db::bash_tasks::get_bash_task(
        &conn,
        &stored.record.harness,
        &stored.record.session_id,
        &stored.record.task_id,
    )
    .map_err(|_| invalid())?
    .ok_or_else(invalid)?;
    let metadata: crate::bash_background::persistence::PersistedTask =
        serde_json::from_str(&row.metadata).map_err(|_| invalid())?;
    #[cfg(unix)]
    if metadata.remote.is_some() {
        return Err(Refusal::new(
            Code::AuthorityUnavailable,
            "guard_unsupported",
        ));
    }
    let lifecycle = stored
        .record
        .launch
        .run
        .as_ref()
        .unwrap_or(&stored.record.launch.authority);
    if !matches!(row.status.as_str(), "starting" | "running")
        || !matches!(
            metadata.status,
            crate::bash_background::BgTaskStatus::Starting
                | crate::bash_background::BgTaskStatus::Running
        )
        || row.harness != stored.record.harness
        || row.session_id != stored.record.session_id
        || row.task_id != stored.record.task_id
        || metadata.harness.as_deref() != Some(stored.record.harness.as_str())
        || metadata.session_id != row.session_id
        || metadata.task_id != row.task_id
        || metadata.child_pid != Some(seal.child_pid)
        || metadata.child_start_time_ms != Some(seal.child_start_time_ms)
        || metadata.scope.as_ref() != Some(lifecycle)
        || row.pid != Some(i64::from(seal.child_pid))
        || row.project_key
            != crate::path_identity::project_scope_key(std::path::Path::new(
                &stored.record.project_root,
            ))
        || metadata
            .project_root
            .as_deref()
            .map(crate::path_identity::project_scope_key)
            .as_deref()
            != Some(row.project_key.as_str())
    {
        return Err(invalid());
    }
    Ok(Redeemed {
        session_id: stored.record.session_id,
        task_id: stored.record.task_id,
        project_root: stored.record.project_root,
        read_repository: None,
        lifecycle_scope: Some(lifecycle.clone()),
        launch: Some(stored.record.launch),
    })
}

pub(crate) fn speech_for_task(session: &str, task: &str) -> Vec<Arc<SpeechTicket>> {
    registry()
        .iter()
        .filter(|entry| {
            entry.session_id == session && matches!(&entry.owner,Owner::Task(id)if id==task)
        })
        .filter_map(|entry| entry.speech.clone())
        .filter(|speech| speech.stored.load(Ordering::SeqCst))
        .collect()
}
