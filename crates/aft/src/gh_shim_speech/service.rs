use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use serde_json::Value;
use subc_protocol::scope::ScopeStamp;
use subc_protocol::session::ModuleControlResponseToModule;

use super::model::{Binding, LaunchAuthority, PolicySnapshot};
use super::scope::{validate_scope_identity, Answer, ValidatedEnd};
use super::{Code, Refusal, ScopeRole};
use crate::db::access_grants::ScopeKey;
use crate::db::gh_speech::Store;
use crate::gh_shim_relay::{facade_reply, GhRelay};

type Describe = Arc<
    dyn Fn(
            ScopeStamp,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<ModuleControlResponseToModule, String>>
                    + Send,
            >,
        > + Send
        + Sync,
>;
type EndNotice = Arc<dyn Fn(ScopeStamp) + Send + Sync>;

pub(crate) fn binding_refusal(code: Code, reason: &str, detail: impl Into<String>) -> Refusal {
    let detail = detail.into();
    log::warn!(
        "gh speech binding verification refused: {}",
        serde_json::json!({"reason": reason, "detail": detail})
    );
    let mut refusal = Refusal::new(code, reason);
    refusal.detail = Some(detail);
    refusal
}

fn binding_unverifiable(detail: impl Into<String>) -> Refusal {
    binding_refusal(Code::AuthorityUnavailable, "binding_unverifiable", detail)
}

#[cfg(test)]
#[derive(Clone)]
pub(crate) struct TestPartners {
    pub(crate) seal_failure: Option<Refusal>,
    pub(crate) expect_placed: Option<&'static str>,
    pub(crate) run_placed: Option<&'static str>,
    pub(crate) reserved_send_placed: bool,
    pub(crate) bindings: Arc<dyn Fn(&str, &str) -> Result<Value, Refusal> + Send + Sync>,
    pub(crate) reserve: Arc<
        dyn Fn(
                &str,
                &str,
                &ScopeStamp,
                Value,
            ) -> Result<super::partner::ReservedBotRequest, Refusal>
            + Send
            + Sync,
    >,
    pub(crate) before_final_gate: Option<Arc<dyn Fn() + Send + Sync>>,
}

static RUN: OnceLock<Option<String>> = OnceLock::new();
pub(crate) fn run_nonce() -> Option<&'static str> {
    RUN.get_or_init(|| {
        let mut bytes = [0; 16];
        getrandom::fill(&mut bytes).ok()?;
        Some(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
    })
    .as_deref()
}

#[derive(Clone, Debug)]
pub(crate) struct LaunchSeed {
    pub(crate) launch: Result<LaunchAuthority, Refusal>,
    pub(crate) lifecycle: Option<ScopeStamp>,
    pub(crate) session: String,
    pub(crate) harness: String,
    pub(crate) project_root: String,
}

#[derive(Clone)]
pub(crate) struct LaunchContext {
    pub(crate) seed: LaunchSeed,
    pub(crate) service: Arc<Service>,
}

#[derive(Clone)]
pub(crate) struct PreparedLaunch {
    pub(crate) context: LaunchContext,
    pub(crate) policy: Option<PolicySnapshot>,
    pub(crate) classification: Option<PolicySnapshot>,
    pub(crate) bindings: BTreeMap<String, Binding>,
    pub(crate) denial: Option<Refusal>,
    pub(crate) provisioned: bool,
}

thread_local! {static LAUNCH:RefCell<Option<LaunchContext>>=const{RefCell::new(None)};}
pub(crate) fn with_launch<T>(
    seed: Option<LaunchSeed>,
    storage: &Path,
    run: impl FnOnce() -> T,
) -> T {
    struct Restore(Option<LaunchContext>);
    impl Drop for Restore {
        fn drop(&mut self) {
            LAUNCH.with(|slot| *slot.borrow_mut() = self.0.take());
        }
    }
    let context =
        seed.and_then(|seed| lookup(storage).map(|service| LaunchContext { seed, service }));
    let _restore = Restore(LAUNCH.with(|slot| slot.replace(context)));
    run()
}
pub(crate) fn current() -> Option<LaunchContext> {
    LAUNCH.with(|slot| slot.borrow().clone())
}

struct Runtime {
    handle: tokio::runtime::Handle,
    relay: Weak<GhRelay>,
    describe: Describe,
    generation: String,
    describe_supported: bool,
}

#[derive(Clone)]
struct Cached {
    sent: Instant,
    generation: String,
    reply: Result<ModuleControlResponseToModule, String>,
}

pub(crate) struct Service {
    pub(crate) store: Store,
    root: PathBuf,
    runtime: Mutex<Option<Runtime>>,
    lease: Mutex<Option<Arc<crate::root_cache::SpeechServingLease>>>,
    lifecycle: Mutex<lifecycle::State>,
    pending_tasks: Mutex<HashSet<String>>,
    pending_ids: Mutex<HashSet<(String, String)>>,
    #[cfg(test)]
    test_partners: Mutex<Option<Arc<TestPartners>>>,
    pending_scopes: Mutex<HashSet<ScopeKey>>,
    pub(crate) admission: Mutex<()>,
    cache: Mutex<HashMap<ScopeKey, Cached>>,
    flights: Mutex<HashMap<ScopeKey, Arc<tokio::sync::Mutex<()>>>>,
    on_end: Mutex<Option<EndNotice>>,
    notified: Mutex<HashSet<ScopeKey>>,
}

static SERVICES: OnceLock<Mutex<HashMap<PathBuf, Arc<Service>>>> = OnceLock::new();
mod lifecycle;
#[cfg(all(test, unix))]
pub(crate) use lifecycle::RuntimeGuard;

fn new_service(root: PathBuf) -> Arc<Service> {
    Arc::new(Service {
        store: Store::new(root.join("aft.db")),
        root,
        runtime: Mutex::new(None),
        lease: Mutex::new(None),
        lifecycle: Mutex::new(Default::default()),
        pending_tasks: Mutex::new(HashSet::new()),
        pending_ids: Mutex::new(HashSet::new()),
        #[cfg(test)]
        test_partners: Mutex::new(None),
        pending_scopes: Mutex::new(HashSet::new()),
        admission: Mutex::new(()),
        cache: Mutex::new(HashMap::new()),
        flights: Mutex::new(HashMap::new()),
        on_end: Mutex::new(None),
        notified: Mutex::new(HashSet::new()),
    })
}
impl std::fmt::Debug for Service {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpeechService")
            .field("store", &self.store.path())
            .finish_non_exhaustive()
    }
}
fn key(root: &Path) -> PathBuf {
    std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf())
}
pub(crate) fn lookup(root: &Path) -> Option<Arc<Service>> {
    SERVICES
        .get()?
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(&key(root))
        .cloned()
}

pub(crate) fn start(
    root: &Path,
    relay: &Arc<GhRelay>,
    describe: Describe,
    generation: String,
    describe_supported: bool,
) -> Arc<Service> {
    let root = key(root);
    if let Err(error) = crate::db::open(&root.join("aft.db")) {
        log::warn!("speech authority initialization unavailable: {error}");
    }
    let mut services = SERVICES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    // A live or retired runtime is never reconfigured into its successor.
    // Keeping separate service identities fences late callbacks from the old run.
    let service = services
        .get(&root)
        .filter(|service| {
            !service
                .lifecycle
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .managed
        })
        .cloned()
        .unwrap_or_else(|| new_service(root.clone()));
    {
        let mut state = service
            .lifecycle
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.managed = true;
        state.accepting = true;
    }
    services.insert(root.clone(), service.clone());
    *service
        .runtime
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = Some(Runtime {
        handle: tokio::runtime::Handle::current(),
        relay: Arc::downgrade(relay),
        describe,
        generation,
        describe_supported,
    });
    service
        .cache
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clear();
    service
        .flights
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clear();
    service
        .notified
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clear();
    service.try_serve();
    service
}

impl Service {
    pub(crate) fn try_serve(&self) -> bool {
        let lifecycle = self
            .lifecycle
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if !lifecycle.accepting {
            return false;
        }
        if self
            .runtime
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .is_none()
        {
            return false;
        }
        let mut held = self.lease.lock().unwrap_or_else(|error| error.into_inner());
        if held.is_none() {
            *held = crate::root_cache::acquire_speech_serving_lease(&self.root).ok();
        }
        held.is_some()
    }

    pub(crate) fn set_end_notice(&self, notice: EndNotice) {
        *self
            .on_end
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(notice);
    }
    pub(crate) fn generation(&self) -> String {
        self.runtime
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .map(|runtime| runtime.generation.clone())
            .unwrap_or_default()
    }
    pub(crate) fn check_pending(
        &self,
        incarnation: Option<&str>,
        launch: &LaunchAuthority,
        session: &str,
        task: &str,
    ) -> Result<(), Refusal> {
        if self.identity_pending(session, task) {
            return Err(Refusal::new(
                Code::AuthorityUnavailable,
                "revocation_pending",
            ));
        }
        if incarnation.is_some_and(|inc| {
            self.pending_tasks
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .contains(inc)
        }) {
            return Err(Refusal::new(
                Code::AuthorityUnavailable,
                "revocation_pending",
            ));
        }
        for (role, stamp) in launch.required_scopes() {
            if self
                .pending_scopes
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .contains(&ScopeKey::from_stamp(stamp))
            {
                return Err(Refusal::scoped(Code::ScopeEnded, "ended", role));
            }
        }
        self.store.check_launch(launch)
    }

    pub(crate) fn revoke(self: &Arc<Self>, incarnation: &str, reason: &str) -> Result<(), Refusal> {
        let work = self.track_finalization();
        let _gate = self
            .admission
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        self.pending_tasks
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(incarnation.into());
        let result = self.store.revoke(incarnation, reason);
        if result.is_ok() {
            self.pending_tasks
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .remove(incarnation);
        } else {
            let service = Arc::clone(self);
            let inc = incarnation.to_string();
            let reason = reason.to_string();
            std::thread::spawn(move || {
                let _work = work;
                let mut delay = Duration::from_millis(100);
                loop {
                    std::thread::sleep(delay);
                    if service.store.revoke(&inc, &reason).is_ok() {
                        service
                            .pending_tasks
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .remove(&inc);
                        break;
                    }
                    delay = (delay * 2).min(Duration::from_secs(5));
                }
            });
        }
        result
    }

    pub(crate) fn record_end(
        self: &Arc<Self>,
        scope: &ScopeStamp,
        proof: ValidatedEnd,
    ) -> Result<(), Refusal> {
        let work = self.track_finalization();
        let key = ScopeKey::from_stamp(scope);
        if proof.key() != &key || proof.evidence().get("scope") != Some(&serde_json::json!(scope)) {
            return Err(Refusal::scoped(
                Code::ScopeUnknown,
                "unverifiable",
                ScopeRole::Authority,
            ));
        }
        let _gate = self
            .admission
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if proof.evidence().get("generation").and_then(Value::as_str)
            != Some(self.generation().as_str())
        {
            return Err(Refusal::scoped(
                Code::ScopeUnknown,
                "unverifiable",
                super::ScopeRole::Authority,
            ));
        }
        self.pending_scopes
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(key.clone());
        let result = self.store.record_end(&proof);
        if result.is_ok() {
            self.pending_scopes
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .remove(&key);
            if self
                .notified
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .insert(key.clone())
            {
                if let Some(notice) = self
                    .on_end
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .clone()
                {
                    notice(scope.clone());
                }
            }
        } else {
            let service = Arc::clone(self);
            let scope = scope.clone();
            std::thread::spawn(move || {
                let _work = work;
                let mut delay = Duration::from_millis(100);
                loop {
                    std::thread::sleep(delay);
                    if service.store.record_end(&proof).is_ok() {
                        service
                            .pending_scopes
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .remove(&key);
                        if service
                            .notified
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .insert(key.clone())
                        {
                            if let Some(notice) = service
                                .on_end
                                .lock()
                                .unwrap_or_else(|error| error.into_inner())
                                .clone()
                            {
                                notice(scope);
                            }
                        }
                        break;
                    }
                    delay = (delay * 2).min(Duration::from_secs(5));
                }
            });
        }
        result
    }
}

impl Service {
    pub(crate) fn record_rpc_end(
        self: &Arc<Self>,
        scope: &ScopeStamp,
        role: ScopeRole,
        selector: &subc_client_rs::ScopeSelector,
        request_generation: &str,
        source: super::scope::ScopedRpcEndSource,
    ) -> Result<(), Refusal> {
        let proof = ValidatedEnd::scoped_rpc(
            scope,
            role,
            selector,
            request_generation,
            &self.generation(),
            source,
        )?;
        self.record_end(scope, proof)
    }

    pub(crate) fn observe_unqualified_end(self: &Arc<Self>, scopes: Vec<(ScopeRole, ScopeStamp)>) {
        let handle = self
            .runtime
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .map(|runtime| runtime.handle.clone());
        let Some(handle) = handle else { return };
        let work = self.track_finalization();
        for (_, scope) in &scopes {
            self.invalidate(scope);
        }
        let service = self.clone();
        // An unqualified provider error is not proof that a particular scope ended.
        // Refresh descriptions asynchronously without delaying the original GitHub reply.
        let _ = handle.spawn(async move {
            let _work = work;
            let deadline = tokio::time::Instant::now() + Duration::from_millis(9_750);
            for (role, scope) in scopes {
                let _ = service.check_scope(&scope, role, deadline).await;
            }
        });
    }
}

#[cfg(test)]
mod tests;

impl LaunchContext {
    pub(crate) fn prepare(&self) -> PreparedLaunch {
        let _work = self.service.track_finalization();
        let started = Instant::now();
        let mut prepared = PreparedLaunch {
            context: self.clone(),
            policy: None,
            classification: None,
            bindings: BTreeMap::new(),
            denial: None,
            provisioned: false,
        };
        let (snapshot, desired) = match self.service.register_accepted_policy() {
            Ok(Some(value)) => value,
            Ok(None) => {
                if let Ok(Some(old)) = self.service.store.latest_policy() {
                    prepared.provisioned = true;
                    prepared.classification = Some(old);
                    prepared.denial = Some(Refusal::new(
                        Code::AuthorityUnavailable,
                        "current_manifest_unverifiable",
                    ));
                }
                return prepared;
            }
            Err(error) => {
                prepared.provisioned = true;
                prepared.classification = self.service.store.latest_policy().ok().flatten();
                prepared.denial = Some(error);
                return prepared;
            }
        };
        prepared.provisioned = true;
        prepared.classification = Some(snapshot.clone());
        let launch = match &self.seed.launch {
            Ok(launch) => launch,
            Err(error) => {
                prepared.denial = Some(error.clone());
                return prepared;
            }
        };
        let handle = {
            let guard = self
                .service
                .runtime
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let Some(runtime) = guard.as_ref() else {
                prepared.denial = Some(binding_unverifiable("no_runtime"));
                return prepared;
            };
            runtime.handle.clone()
        };
        if tokio::runtime::Handle::try_current().is_ok() {
            prepared.denial = Some(binding_unverifiable("inside_tokio_runtime"));
            return prepared;
        }
        let agent = match launch.agent() {
            Ok(agent) => agent,
            Err(error) => {
                prepared.denial = Some(error);
                return prepared;
            }
        };
        let result = handle.block_on(async {
            tokio::time::timeout(
                Duration::from_secs(1).saturating_sub(started.elapsed()),
                async {
                    let reply = self
                        .service
                        .read_bindings(&self.seed.project_root, &self.seed.session, agent)
                        .await?;
                    parse_bindings(&desired, &facade_reply(&reply), agent)
                },
            )
            .await
            .map_err(|_| binding_unverifiable("start_binding_read_timeout"))?
        });
        match result {
            Ok(bindings) => {
                prepared.policy = Some(snapshot);
                prepared.bindings = bindings;
            }
            Err(error) => prepared.denial = Some(error),
        }
        prepared
    }
}

impl Service {
    /// Observe only a manifest already accepted by the shim. The shim owns its
    /// JSON records; the daemon independently verifies them before writing aft.db.
    pub(crate) fn register_accepted_policy(
        &self,
    ) -> Result<Option<(PolicySnapshot, BTreeMap<String, String>)>, Refusal> {
        let Some((snapshot, desired, accepted_version)) = crate::gh_shim::capture_speech_policy()?
        else {
            return Ok(None);
        };
        self.store.initialize_policy(&snapshot, accepted_version)?;
        self.store.advance_policy(&snapshot)?;
        Ok(Some((snapshot, desired)))
    }
}

pub(crate) fn parse_bindings(
    desired: &BTreeMap<String, String>,
    reply: &Value,
    agent: &str,
) -> Result<BTreeMap<String, Binding>, Refusal> {
    let invalid = || binding_unverifiable("invalid_binding");
    let reply = reply
        .get("result")
        .filter(|result| result.get("repo_binding_generation").is_some())
        .unwrap_or(reply);
    let generation = reply
        .get("repo_binding_generation")
        .and_then(Value::as_u64)
        .ok_or_else(|| binding_unverifiable("missing_repo_binding_generation"))?;
    let rows = reply
        .get("bindings")
        .and_then(Value::as_array)
        .ok_or_else(invalid)?;
    let mut found = BTreeMap::new();
    let mut seen = HashSet::new();
    for row in rows {
        let repo = row
            .get("repository")
            .and_then(Value::as_str)
            .ok_or_else(invalid)?
            .to_ascii_lowercase();
        let current = row
            .get("agent_id")
            .and_then(Value::as_str)
            .ok_or_else(invalid)?;
        let signed = desired
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(&repo))
            .map(|(_, value)| value.as_str());
        if !seen.insert(repo.clone()) || signed != Some(current) || current != agent {
            return Err(binding_refusal(Code::IdentityConflict, "binding_changed", format!(
                "binding_identity_mismatch: repository={repo:?} signed_agent={signed:?} returned_agent={current:?} launch_agent={agent:?}"
            )));
        }
        if current == agent {
            let handle = row
                .get("app_handle_id")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(invalid)?;
            found.insert(
                repo,
                Binding {
                    app_handle_id: handle.into(),
                    repo_binding_generation: generation,
                },
            );
        }
    }
    if found.is_empty() {
        let expected: Vec<_> = desired
            .iter()
            .filter(|(_, owner)| owner.as_str() == agent)
            .map(|(repo, _)| repo.as_str())
            .collect();
        return Err(binding_unverifiable(format!(
            "no_binding_row: launch_agent={agent:?} repositories={expected:?}"
        )));
    }
    Ok(found)
}

impl Service {
    pub(crate) async fn check_scope(
        self: &Arc<Self>,
        scope: &ScopeStamp,
        role: ScopeRole,
        deadline: tokio::time::Instant,
    ) -> Result<(), Refusal> {
        if tokio::time::Instant::now() >= deadline {
            return Err(Refusal::scoped(Code::ScopeUnknown, "timeout", role));
        }
        let key = ScopeKey::from_stamp(scope);
        if self
            .pending_scopes
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .contains(&key)
        {
            return Err(Refusal::scoped(Code::ScopeEnded, "ended", role));
        }
        let (generation, describe) = {
            let runtime = self
                .runtime
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let runtime = runtime
                .as_ref()
                .ok_or_else(|| Refusal::scoped(Code::ScopeUnknown, "unsupported", role))?;
            if !runtime.describe_supported {
                return Err(Refusal::scoped(Code::ScopeUnknown, "unsupported", role));
            }
            (runtime.generation.clone(), Arc::clone(&runtime.describe))
        };
        let flight = self
            .flights
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .entry(key.clone())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        let _flight = tokio::time::timeout_at(deadline, flight.lock())
            .await
            .map_err(|_| Refusal::scoped(Code::ScopeUnknown, "timeout", role))?;
        let cached = self
            .cache
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&key)
            .filter(|entry| {
                entry.generation == generation && entry.sent.elapsed() < Duration::from_secs(5)
            })
            .cloned();
        let cached = if let Some(cached) = cached {
            cached
        } else {
            let sent = Instant::now();
            let reply = tokio::time::timeout_at(deadline, describe(scope.clone()))
                .await
                .map_err(|_| Refusal::scoped(Code::ScopeUnknown, "timeout", role))?;
            let cached = Cached {
                sent,
                generation: generation.clone(),
                reply,
            };
            // Definitive ends are irreversible evidence, not replaceable cache entries.
            // Validate the received answer before a newer Live/unknown can supersede it.
            if let Ok(reply) = &cached.reply {
                if let Ok(Answer::Ended(proof)) = validate_scope_identity(
                    scope,
                    role,
                    reply,
                    &self.generation(),
                    &cached.generation,
                    crate::db::call_ledger::now_ms() as u64,
                ) {
                    let _ = self.record_end(scope, proof);
                    return Err(Refusal::scoped(Code::ScopeEnded, "ended", role));
                }
            }
            self.remember_probe(scope, &generation, sent, cached.reply.clone());
            self.cache
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .get(&key)
                .cloned()
                .unwrap_or(cached)
        };
        match self.validate_cached_scope(scope, role, &cached)? {
            Answer::Live(_) => Ok(()),
            Answer::Ended(proof) => {
                let _ = self.record_end(scope, proof);
                Err(Refusal::scoped(Code::ScopeEnded, "ended", role))
            }
        }
    }

    fn validate_cached_scope(
        &self,
        scope: &ScopeStamp,
        role: ScopeRole,
        cached: &Cached,
    ) -> Result<Answer, Refusal> {
        let generation = self.generation();
        if generation != cached.generation {
            return Err(Refusal::scoped(Code::ScopeUnknown, "unverifiable", role));
        }
        let reply = cached
            .reply
            .as_ref()
            .map_err(|_| Refusal::scoped(Code::ScopeUnknown, "error", role))?;
        match validate_scope_identity(
            scope,
            role,
            reply,
            &generation,
            &cached.generation,
            crate::db::call_ledger::now_ms() as u64,
        )? {
            Answer::Live(stamp) => {
                if !stamp.owner_authorized {
                    return Err(Refusal::scoped(Code::ScopeUnknown, "unverifiable", role));
                }
                if cached.sent.elapsed() >= Duration::from_secs(5) {
                    return Err(Refusal::scoped(Code::ScopeUnknown, "timeout", role));
                }
                Ok(Answer::Live(stamp))
            }
            answer => Ok(answer),
        }
    }

    /// Recheck cached evidence after queue/binding waits, without a network call
    /// or a nested database transaction inside the final SQLite send gate.
    pub(crate) fn check_cached_scope(
        &self,
        scope: &ScopeStamp,
        role: ScopeRole,
    ) -> Result<(), Refusal> {
        let cached = self
            .cache
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&ScopeKey::from_stamp(scope))
            .cloned()
            .ok_or_else(|| Refusal::scoped(Code::ScopeUnknown, "timeout", role))?;
        match self.validate_cached_scope(scope, role, &cached)? {
            Answer::Live(_) => Ok(()),
            Answer::Ended(_) => Err(Refusal::scoped(Code::ScopeEnded, "ended", role)),
        }
    }
}

/// The registry has already authorized this exact server-generated task lookup.
pub(crate) fn revoke_task_records(session: &str, task: &str) -> bool {
    crate::gh_shim_ticket::speech_for_task(session, task)
        .into_iter()
        .fold(false, |failed, speech| {
            speech
                .prepared
                .context
                .service
                .revoke_identity(session, task)
                .is_err()
                || failed
        })
}

impl Service {
    pub(crate) fn invalidate(&self, scope: &ScopeStamp) {
        self.cache
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&ScopeKey::from_stamp(scope));
    }
    pub(crate) async fn read_bindings(
        &self,
        project: &str,
        session: &str,
        agent: &str,
    ) -> Result<Value, Refusal> {
        #[cfg(test)]
        if let Some(partners) = self.test_partners() {
            return (partners.bindings)(project, session);
        }
        let relay = {
            let runtime = self
                .runtime
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            runtime.as_ref().and_then(|runtime| runtime.relay.upgrade())
        }
        .ok_or_else(|| binding_unverifiable("relay_unavailable"))?;
        tokio::time::timeout(
            Duration::from_secs(1),
            relay.read_bindings(project, session, agent),
        )
        .await
        .map_err(|_| binding_unverifiable("binding_read_timeout"))?
        .map_err(|error| binding_unverifiable(format!("binding_read_transport: {error:?}")))
    }
    pub(crate) async fn reserve(
        &self,
        project: &str,
        session: &str,
        scope: &ScopeStamp,
        body: Value,
        deadline: tokio::time::Instant,
    ) -> Result<super::partner::ReservedBotRequest, Refusal> {
        #[cfg(test)]
        if let Some(partners) = self.test_partners() {
            return (partners.reserve)(project, session, scope, body);
        }
        let relay = {
            let runtime = self
                .runtime
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            runtime.as_ref().and_then(|runtime| runtime.relay.upgrade())
        }
        .ok_or_else(|| Refusal::new(Code::AuthorityUnavailable, "guard_unsupported"))?;
        relay
            .reserve_bot(project, session, scope, body, deadline)
            .await
    }
}

pub(crate) fn record_service(root: &Path) -> Arc<Service> {
    let root = key(root);
    SERVICES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .entry(root.clone())
        .or_insert_with(|| new_service(root))
        .clone()
}

impl Service {
    pub(crate) fn identity_pending(&self, session: &str, task: &str) -> bool {
        self.pending_ids
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .contains(&(session.into(), task.into()))
    }
    pub(crate) fn revoke_identity(
        self: &Arc<Self>,
        session: &str,
        task: &str,
    ) -> Result<(), Refusal> {
        let work = self.track_finalization();
        let _gate = self
            .admission
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let identity = (session.to_string(), task.to_string());
        let first = self
            .pending_ids
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(identity.clone());
        let attempt = || {
            for inc in self.store.incarnation_for_task(session, task)? {
                self.store.revoke(&inc, "cancelled")?;
            }
            Ok::<_, Refusal>(())
        };
        let result = attempt();
        if result.is_ok() {
            self.pending_ids
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .remove(&identity);
        } else if first {
            let service = Arc::clone(self);
            std::thread::spawn(move || {
                let _work = work;
                let mut delay = Duration::from_millis(100);
                loop {
                    std::thread::sleep(delay);
                    let result = service
                        .store
                        .incarnation_for_task(&identity.0, &identity.1)
                        .and_then(|incs| {
                            for inc in incs {
                                service.store.revoke(&inc, "cancelled")?;
                            }
                            Ok(())
                        });
                    if result.is_ok() {
                        service
                            .pending_ids
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .remove(&identity);
                        break;
                    }
                    delay = (delay * 2).min(Duration::from_secs(5));
                }
            });
        }
        result
    }
}

pub(crate) fn revoke_at(path: &Path, session: &str, task: &str) -> bool {
    let Some(root) = path.parent() else {
        return true;
    };
    record_service(root).revoke_identity(session, task).is_err()
}

pub(crate) fn revocation_pending(session: &str, task: &str) -> bool {
    SERVICES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .values()
        .any(|service| service.identity_pending(session, task))
}

impl Service {
    pub(crate) fn remember_probe(
        &self,
        scope: &ScopeStamp,
        generation: &str,
        sent: Instant,
        reply: Result<ModuleControlResponseToModule, String>,
    ) {
        if self.generation() != generation {
            return;
        }
        let key = ScopeKey::from_stamp(scope);
        let mut cache = self.cache.lock().unwrap_or_else(|error| error.into_inner());
        if cache.get(&key).is_some_and(|entry| entry.sent > sent) {
            return;
        }
        cache.insert(
            key,
            Cached {
                sent,
                generation: generation.into(),
                reply,
            },
        );
    }
}

impl Service {
    pub(crate) fn require_posting_guards(&self, codemode: bool) -> Result<(), Refusal> {
        #[cfg(test)]
        if let Some(partners) = self.test_partners() {
            return if partners.expect_placed.is_some()
                && (!codemode || partners.run_placed.is_some())
                && partners.reserved_send_placed
            {
                Ok(())
            } else {
                Err(Refusal::new(
                    Code::AuthorityUnavailable,
                    "guard_unsupported",
                ))
            };
        }
        super::partner::require_plex_guards(codemode)?;
        super::partner::require_reserved_send()
    }

    #[cfg(test)]
    pub(crate) fn test_seal_failure(&self) -> Option<Refusal> {
        self.test_partners()
            .and_then(|partners| partners.seal_failure.clone())
    }

    #[cfg(all(test, debug_assertions, any(target_os = "linux", target_os = "macos")))]
    pub(crate) fn install_test_partners(&self, partners: Arc<TestPartners>) {
        *self
            .test_partners
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(partners);
    }

    #[cfg(test)]
    pub(crate) fn test_partners(&self) -> Option<Arc<TestPartners>> {
        self.test_partners
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    #[cfg(all(test, debug_assertions, any(target_os = "linux", target_os = "macos")))]
    pub(crate) fn expire_test_scope_cache(&self) {
        for cached in self
            .cache
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .values_mut()
        {
            cached.sent -= Duration::from_secs(6);
        }
    }

    #[cfg(test)]
    pub(crate) fn test_before_final_gate(&self) {
        if let Some(partners) = self.test_partners() {
            if let Some(hook) = &partners.before_final_gate {
                hook();
            }
        }
    }
}
