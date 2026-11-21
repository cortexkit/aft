use crate::bash_background::persistence::PersistedTask;
use crate::gh_shim_speech::{self, service::Service, store::Store, Code, Refusal, ScopeRole};
use crate::gh_shim_ticket;
use serde_json::Value;
use std::{path::Path, sync::Arc};
use subc_protocol::scope::ScopeStamp;

pub use gh_shim_speech::model::authority_types::{
    AuthorityRecord, Binding, ExecutionSeal, PolicySnapshot,
};
pub use gh_shim_speech::partner::partner_types::{
    QueuedBotResponse, QueuedResponseError, ReservedBotRequest, RunSelector,
};
pub use gh_shim_speech::scope::scope_end_type::ScopedRpcEndSource;
pub use gh_shim_speech::service::launch_types::{LaunchContext, PreparedLaunch};
pub use gh_shim_speech::store::store_types::{GateError, StoredAuthority};
pub use gh_shim_ticket::ticket_type::SpeechTicket;

#[cfg(all(debug_assertions, any(target_os = "linux", target_os = "macos")))]
pub use crate::gh_shim::speech_dispatch_support::SpeechManifest;

pub const START_VIEW_ENV: &str = gh_shim_speech::start_view::START_VIEW_ENV;

pub fn speech_for_ticket(text: &str) -> Option<Arc<SpeechTicket>> {
    gh_shim_ticket::speech_for_ticket(text)
}

pub fn speech_for_task(session: &str, task: &str) -> Vec<Arc<SpeechTicket>> {
    gh_shim_ticket::speech_for_task(session, task)
}

pub fn speech_digest(text: &str) -> Option<[u8; 32]> {
    gh_shim_ticket::speech_digest(text)
}

pub fn redeem_speech_cold(
    ticket: &str,
    service: &Service,
) -> Result<gh_shim_ticket::Redeemed, Refusal> {
    gh_shim_ticket::redeem_speech_cold(ticket, service)
}

pub fn promote_speech_task(session: &str, task: &str) {
    gh_shim_ticket::promote_speech_task(session, task);
}

pub fn seal_spawn(metadata: &PersistedTask) {
    gh_shim_ticket::seal_spawn(metadata);
}

pub fn spawn_ticket_guard(ticket: Option<&str>) -> impl Drop {
    gh_shim_ticket::spawn_ticket_guard(ticket)
}

pub fn mark_spawn_background(background: bool) {
    gh_shim_ticket::mark_spawn_background(background);
}

pub fn lookup(root: &Path) -> Option<Arc<Service>> {
    gh_shim_speech::service::lookup(root)
}

pub fn binding_refusal(code: Code, reason: &str, detail: impl Into<String>) -> Refusal {
    gh_shim_speech::service::binding_refusal(code, reason, detail)
}

pub fn require_reserved_send() -> Result<(), Refusal> {
    gh_shim_speech::partner::require_reserved_send()
}

pub fn reserved_bot_request(
    commit: impl FnOnce() -> Result<QueuedBotResponse, Refusal> + Send + 'static,
) -> ReservedBotRequest {
    ReservedBotRequest::new(commit)
}

pub fn queued_bot_response(
    response: impl std::future::Future<Output = Result<Value, QueuedResponseError>> + Send + 'static,
) -> QueuedBotResponse {
    QueuedBotResponse::new(response)
}

#[cfg(all(debug_assertions, any(target_os = "linux", target_os = "macos")))]
pub fn reserved_bot_request_for_test(
    commit: impl FnOnce() -> Result<QueuedBotResponse, Refusal> + Send + 'static,
) -> ReservedBotRequest {
    ReservedBotRequest::for_test(commit)
}

#[cfg(all(debug_assertions, any(target_os = "linux", target_os = "macos")))]
pub fn queued_bot_response_for_test(reply: Value) -> QueuedBotResponse {
    QueuedBotResponse::for_test(reply)
}

impl LaunchContext {
    pub fn for_test(seed: gh_shim_speech::service::LaunchSeed, service: Arc<Service>) -> Self {
        Self { seed, service }
    }

    pub fn seed(&self) -> &gh_shim_speech::service::LaunchSeed {
        &self.seed
    }

    pub fn service(&self) -> &Arc<Service> {
        &self.service
    }

    pub fn test_prepare(&self) -> PreparedLaunch {
        self.prepare()
    }
}

impl SpeechTicket {
    pub fn prepared(&self) -> &PreparedLaunch {
        &self.prepared
    }

    pub fn incarnation(&self) -> &str {
        &self.incarnation
    }

    pub fn is_active(&self) -> bool {
        self.active.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn failure(&self) -> std::sync::LockResult<std::sync::MutexGuard<'_, Option<Refusal>>> {
        self.failure.lock()
    }
}

impl PreparedLaunch {
    pub fn context(&self) -> &LaunchContext {
        &self.context
    }

    pub fn policy(&self) -> Option<&PolicySnapshot> {
        self.policy.as_ref()
    }

    pub fn denial(&self) -> Option<&Refusal> {
        self.denial.as_ref()
    }
}

impl Binding {
    pub fn app_handle_id(&self) -> &str {
        &self.app_handle_id
    }

    pub fn repo_binding_generation(&self) -> u64 {
        self.repo_binding_generation
    }
}

impl Service {
    pub fn test_record_rpc_end(
        self: &Arc<Self>,
        scope: &ScopeStamp,
        role: ScopeRole,
        selector: &subc_client_rs::ScopeSelector,
        request_generation: &str,
        source: ScopedRpcEndSource,
    ) -> Result<(), Refusal> {
        self.record_rpc_end(scope, role, selector, request_generation, source)
    }
}

impl Store {
    pub fn test_path(&self) -> &Path {
        self.path()
    }

    pub fn test_connect(&self) -> Result<crate::db::TrackedConnection, Refusal> {
        self.connect()
    }

    pub fn test_capture(&self, record: &AuthorityRecord) -> Result<(), Refusal> {
        self.capture(record)
    }

    pub fn test_seal(&self, incarnation: &str, seal: &ExecutionSeal) -> Result<(), Refusal> {
        self.seal(incarnation, seal)
    }

    pub fn test_get(&self, incarnation: &str) -> Result<StoredAuthority, Refusal> {
        self.get(incarnation)
    }

    pub fn test_prepare_request(
        &self,
        nonce: &str,
        incarnation: &str,
        payload_digest: &[u8; 32],
        expect_json: &str,
    ) -> Result<(), Refusal> {
        self.prepare_request(nonce, incarnation, payload_digest, expect_json)
    }
}

impl StoredAuthority {
    pub fn record(&self) -> &AuthorityRecord {
        &self.record
    }
}

impl ExecutionSeal {
    pub fn for_test(child_pid: u32, child_start_time_ms: u64, birth_identity: String) -> Self {
        Self {
            child_pid,
            child_start_time_ms,
            birth_identity,
        }
    }
}

impl AuthorityRecord {
    pub fn unsealed_for_test(
        incarnation: String,
        minting_run: String,
        harness: String,
        session_id: String,
        task_id: String,
        project_root: String,
        launch: gh_shim_speech::model::LaunchAuthority,
    ) -> Self {
        Self {
            incarnation,
            minting_run,
            harness,
            session_id,
            task_id,
            project_root,
            launch,
            policy: None,
            bindings: Default::default(),
            seal: None,
            ineligible_reason: None,
        }
    }
}
