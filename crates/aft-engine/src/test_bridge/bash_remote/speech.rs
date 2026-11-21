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

pub use gh_shim_speech::service::test_partners_fixture::TestPartners;

type Bindings = Arc<dyn Fn(&str, &str) -> Result<Value, Refusal> + Send + Sync>;
type Reservation = Arc<
    dyn Fn(&str, &str, &ScopeStamp, Value) -> Result<ReservedBotRequest, Refusal> + Send + Sync,
>;

impl TestPartners {
    pub fn for_test(
        seal_failure: Option<Refusal>,
        expect_placed: Option<&'static str>,
        run_placed: Option<&'static str>,
        reserved_send_placed: bool,
        bindings: Bindings,
        reserve: Reservation,
        before_final_gate: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> Self {
        Self {
            seal_failure,
            expect_placed,
            run_placed,
            reserved_send_placed,
            bindings,
            reserve,
            before_final_gate,
        }
    }

    pub fn bindings(&self) -> &Bindings {
        &self.bindings
    }
    pub fn bindings_mut(&mut self) -> &mut Bindings {
        &mut self.bindings
    }
    pub fn reserve(&self) -> &Reservation {
        &self.reserve
    }
    pub fn reserve_mut(&mut self) -> &mut Reservation {
        &mut self.reserve
    }
    pub fn before_final_gate_mut(&mut self) -> &mut Option<Arc<dyn Fn() + Send + Sync>> {
        &mut self.before_final_gate
    }
}

impl Service {
    #[cfg(all(debug_assertions, any(target_os = "linux", target_os = "macos")))]
    pub fn test_install_partners(&self, partners: Arc<TestPartners>) {
        self.install_test_partners(partners);
    }

    pub fn test_partners_for_fixture(&self) -> Option<Arc<TestPartners>> {
        self.test_partners()
    }

    #[cfg(all(debug_assertions, any(target_os = "linux", target_os = "macos")))]
    pub fn test_expire_scope_cache(&self) {
        self.expire_test_scope_cache();
    }
}

impl ReservedBotRequest {
    pub fn test_send(self) -> Result<QueuedBotResponse, Refusal> {
        self.send()
    }
}

impl QueuedBotResponse {
    pub async fn test_response(self) -> Result<Value, QueuedResponseError> {
        self.response().await
    }
}

pub trait StoreTestExt {
    fn get(&self, incarnation: &str) -> Result<StoredAuthority, Refusal>;
    fn connect(&self) -> Result<crate::db::TrackedConnection, Refusal>;
    fn find_digest(&self, digest: &[u8; 32], current_run: &str)
        -> Result<StoredAuthority, Refusal>;
    fn revoke(&self, incarnation: &str, reason: &str) -> Result<(), Refusal>;
    fn seal(&self, incarnation: &str, seal: &ExecutionSeal) -> Result<(), Refusal>;
    fn advance_policy(&self, policy: &PolicySnapshot) -> Result<i64, Refusal>;
    fn record_end(&self, proof: &gh_shim_speech::scope::ValidatedEnd) -> Result<(), Refusal>;
    fn pending_legacy_for_identity(&self, session: &str, task: &str) -> Result<u64, Refusal>;
    fn final_legacy_gate<T>(
        &self,
        incarnation: &str,
        nonce: &str,
        expected_generation: i64,
        policy_generation: i64,
        current_policy: &PolicySnapshot,
        forwarding_run: &str,
        enqueue: impl FnOnce() -> Result<T, Refusal>,
    ) -> Result<T, GateError>;
}

impl StoreTestExt for Store {
    fn get(&self, incarnation: &str) -> Result<StoredAuthority, Refusal> {
        Store::get(self, incarnation)
    }
    fn connect(&self) -> Result<crate::db::TrackedConnection, Refusal> {
        Store::connect(self)
    }
    fn find_digest(
        &self,
        digest: &[u8; 32],
        current_run: &str,
    ) -> Result<StoredAuthority, Refusal> {
        Store::find_digest(self, digest, current_run)
    }
    fn revoke(&self, incarnation: &str, reason: &str) -> Result<(), Refusal> {
        Store::revoke(self, incarnation, reason)
    }
    fn seal(&self, incarnation: &str, seal: &ExecutionSeal) -> Result<(), Refusal> {
        Store::seal(self, incarnation, seal)
    }
    fn advance_policy(&self, policy: &PolicySnapshot) -> Result<i64, Refusal> {
        Store::advance_policy(self, policy)
    }
    fn record_end(&self, proof: &gh_shim_speech::scope::ValidatedEnd) -> Result<(), Refusal> {
        Store::record_end(self, proof)
    }
    fn pending_legacy_for_identity(&self, session: &str, task: &str) -> Result<u64, Refusal> {
        Store::pending_legacy_for_identity(self, session, task)
    }
    fn final_legacy_gate<T>(
        &self,
        incarnation: &str,
        nonce: &str,
        expected_generation: i64,
        policy_generation: i64,
        current_policy: &PolicySnapshot,
        forwarding_run: &str,
        enqueue: impl FnOnce() -> Result<T, Refusal>,
    ) -> Result<T, GateError> {
        Store::final_legacy_gate(
            self,
            incarnation,
            nonce,
            expected_generation,
            policy_generation,
            current_policy,
            forwarding_run,
            enqueue,
        )
    }
}

pub trait ServiceTestExt {
    fn invalidate(&self, scope: &ScopeStamp);
    fn register_accepted_policy(
        &self,
    ) -> Result<Option<(PolicySnapshot, std::collections::BTreeMap<String, String>)>, Refusal>;
}

impl ServiceTestExt for Service {
    fn invalidate(&self, scope: &ScopeStamp) {
        Service::invalidate(self, scope);
    }
    fn register_accepted_policy(
        &self,
    ) -> Result<Option<(PolicySnapshot, std::collections::BTreeMap<String, String>)>, Refusal> {
        Service::register_accepted_policy(self)
    }
}

pub trait RefusalTestExt: Sized {
    fn scoped(code: Code, reason: impl Into<String>, role: ScopeRole) -> Self;
    fn wire_data(&self) -> Value;
    fn detail(&self) -> &Option<String>;
    fn retry_before_send(&self) -> bool;
    fn retry_before_send_mut(&mut self) -> &mut bool;
}

impl RefusalTestExt for Refusal {
    fn scoped(code: Code, reason: impl Into<String>, role: ScopeRole) -> Self {
        Refusal::scoped(code, reason, role)
    }
    fn wire_data(&self) -> Value {
        Refusal::wire_data(self)
    }
    fn detail(&self) -> &Option<String> {
        &self.detail
    }
    fn retry_before_send(&self) -> bool {
        self.retry_before_send
    }
    fn retry_before_send_mut(&mut self) -> &mut bool {
        &mut self.retry_before_send
    }
}

pub trait RouteScopesTestExt {
    fn origin_live(
        &self,
        origin: Option<&gh_shim_speech::model::RouteOrigin>,
        stamp: &ScopeStamp,
    ) -> bool;
}

impl RouteScopesTestExt for crate::gh_shim_relay::RouteScopes {
    fn origin_live(
        &self,
        origin: Option<&gh_shim_speech::model::RouteOrigin>,
        stamp: &ScopeStamp,
    ) -> bool {
        Self::origin_live(self, origin, stamp)
    }
}

pub trait LaunchAuthorityTestExt {
    fn origin(&self) -> &Option<gh_shim_speech::model::RouteOrigin>;
    fn origin_mut(&mut self) -> &mut Option<gh_shim_speech::model::RouteOrigin>;
    fn head_origin(&self) -> &Option<gh_shim_speech::model::RouteOrigin>;
    fn authority(&self) -> &ScopeStamp;
    fn required_scopes(&self) -> Vec<(ScopeRole, &ScopeStamp)>;
}

impl LaunchAuthorityTestExt for gh_shim_speech::model::LaunchAuthority {
    fn origin(&self) -> &Option<gh_shim_speech::model::RouteOrigin> {
        &self.origin
    }
    fn origin_mut(&mut self) -> &mut Option<gh_shim_speech::model::RouteOrigin> {
        &mut self.origin
    }
    fn head_origin(&self) -> &Option<gh_shim_speech::model::RouteOrigin> {
        &self.head_origin
    }
    fn authority(&self) -> &ScopeStamp {
        &self.authority
    }
    fn required_scopes(&self) -> Vec<(ScopeRole, &ScopeStamp)> {
        Self::required_scopes(self)
    }
}

pub trait RedeemedTestExt {
    fn launch(&self) -> &Option<gh_shim_speech::model::LaunchAuthority>;
}

impl RedeemedTestExt for gh_shim_ticket::Redeemed {
    fn launch(&self) -> &Option<gh_shim_speech::model::LaunchAuthority> {
        &self.launch
    }
}

pub trait SpeechTicketTestExt {
    fn background(&self) -> &std::sync::atomic::AtomicBool;
}

impl SpeechTicketTestExt for SpeechTicket {
    fn background(&self) -> &std::sync::atomic::AtomicBool {
        &self.background
    }
}

pub trait StoredAuthorityTestExt {
    fn generation(&self) -> i64;
    fn phase(&self) -> &String;
}

impl StoredAuthorityTestExt for StoredAuthority {
    fn generation(&self) -> i64 {
        self.generation
    }
    fn phase(&self) -> &String {
        &self.phase
    }
}

pub trait AuthorityRecordTestExt {
    fn launch(&self) -> &gh_shim_speech::model::LaunchAuthority;
    fn launch_mut(&mut self) -> &mut gh_shim_speech::model::LaunchAuthority;
    fn bindings(&self) -> &std::collections::BTreeMap<String, Binding>;
    fn bindings_mut(&mut self) -> &mut std::collections::BTreeMap<String, Binding>;
    fn seal(&self) -> &Option<ExecutionSeal>;
    fn incarnation(&self) -> &String;
}

impl AuthorityRecordTestExt for AuthorityRecord {
    fn launch(&self) -> &gh_shim_speech::model::LaunchAuthority {
        &self.launch
    }
    fn launch_mut(&mut self) -> &mut gh_shim_speech::model::LaunchAuthority {
        &mut self.launch
    }
    fn bindings(&self) -> &std::collections::BTreeMap<String, Binding> {
        &self.bindings
    }
    fn bindings_mut(&mut self) -> &mut std::collections::BTreeMap<String, Binding> {
        &mut self.bindings
    }
    fn seal(&self) -> &Option<ExecutionSeal> {
        &self.seal
    }
    fn incarnation(&self) -> &String {
        &self.incarnation
    }
}
