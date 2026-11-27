//! Background vault delivery; never runs on a tool or route dispatch thread.

use super::manifest_wire::{self as wire, Recorded, Reply, ResultBody};
use super::manifest_wire::{Latest, Notice, Report};
use super::*;
use crate::secret::transport::ManagementSession;
use sha2::{Digest, Sha256};
use std::sync::Mutex;

const PIN_REFRESH_INTERVAL: Duration = Duration::from_secs(5 * 60);
type ReadRoot = dyn Fn()
        -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<trust_root::Reply, ()>> + Send>>
    + Send
    + Sync;

struct RootRefresh<'a> {
    read: &'a ReadRoot,
    // Serialize the read and its publication: a slow older response must not
    // overwrite a later pin read by the periodic or pre-activation refresh.
    serial: tokio::sync::Mutex<()>,
}

impl<'a> RootRefresh<'a> {
    fn new(read: &'a ReadRoot) -> Self {
        Self {
            read,
            serial: tokio::sync::Mutex::new(()),
        }
    }

    async fn refresh(&self, paths: &StatePaths) -> Result<(), Failure> {
        let _guard = self.serial.lock().await;
        let reply = tokio::time::timeout(Duration::from_secs(5), (self.read)())
            .await
            .ok()
            .and_then(Result::ok)
            .ok_or_else(|| Failure::new("policy_root_unavailable", true))?;
        trust_root::publish(paths, reply, unix_seconds())
            .map_err(|_| Failure::new("store_error", true))
    }

    async fn poll(&self, paths: &StatePaths) {
        let mut interval = tokio::time::interval(PIN_REFRESH_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            if let Err(failure) = self.refresh(paths).await {
                failure.record();
            } else {
                let _ = local_report(paths, unix_seconds());
            }
        }
    }
}

#[derive(Clone, Default, Serialize)]
struct Health {
    active_version: Option<u64>,
    newest_version_seen: Option<u64>,
    active_state: Option<&'static str>,
    last_report: Option<Report>,
    last_report_outcome: Option<&'static str>,
    last_refusal_code: Option<&'static str>,
}

impl Health {
    fn observe_version(&mut self, version: Option<u64>) {
        if let Some(version) = version {
            self.newest_version_seen = Some(self.newest_version_seen.unwrap_or(0).max(version));
        }
    }
}

static HEALTH: Mutex<Health> = Mutex::new(Health {
    active_version: None,
    newest_version_seen: None,
    active_state: None,
    last_report: None,
    last_report_outcome: None,
    last_refusal_code: None,
});

#[doc(hidden)]
pub fn health() -> Value {
    serde_json::to_value(
        HEALTH
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone(),
    )
    .unwrap_or(Value::Null)
}

fn digest(bytes: &str) -> String {
    format!("{:x}", Sha256::digest(bytes.as_bytes()))
}

fn problem_reason(problem: &ManifestProblem) -> &'static str {
    match problem {
        ManifestProblem::Missing => "manifest missing",
        ManifestProblem::NoTrustRoot => "no pinned policy root",
        ManifestProblem::RolledBack { .. } => "manifest below high-water version",
        ManifestProblem::BelowFloor { .. } => "manifest below schema floor",
        ManifestProblem::Invalid(reason) if reason.starts_with("untrusted manifest key id") => {
            "untrusted manifest key"
        }
        ManifestProblem::Invalid(reason) if reason.starts_with("retired manifest key id") => {
            "retired manifest key"
        }
        ManifestProblem::Invalid(reason) if reason == "detached signature verification failed" => {
            "detached signature verification failed"
        }
        ManifestProblem::Invalid(reason) if reason == "invalid detached signature encoding" => {
            "invalid detached signature encoding"
        }
        _ => "shim rejected manifest",
    }
}

fn local_report(paths: &StatePaths, now: u64) -> Option<Report> {
    // Use the same status path as gh --status, including its acceptance writes.
    let status = cached_manifest_report_at(paths, now);
    let mut health = HEALTH
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    health.active_version = status.version;
    let envelope = fs::read(&paths.manifest)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<SignedManifest>(&bytes).ok());
    let Some(envelope) = envelope else {
        health.active_version = None;
        health.active_state = Some(if paths.manifest.exists() {
            "no active manifest: malformed"
        } else {
            "no active manifest: missing"
        });
        return None;
    };
    let version = serde_json::from_str::<Value>(&envelope.manifest_bytes)
        .ok()
        .and_then(|manifest| manifest["manifest_version"].as_u64())
        .filter(|version| *version > 0);
    let Some(version) = version else {
        health.active_version = None;
        health.active_state = Some("no active manifest: malformed");
        return None;
    };
    let valid = status.state == Some("valid") && status.version == Some(version);
    health.active_state = Some(if valid { "valid" } else { "invalid" });
    Some(Report {
        version,
        payload_sha256: digest(&envelope.manifest_bytes),
        verdict: if valid { "valid" } else { "invalid" }.into(),
        reason: (!valid).then(|| "shim rejected manifest".into()),
    })
}

fn candidate_report(latest: &Latest, reason: &'static str) -> Report {
    Report {
        version: latest.version,
        payload_sha256: digest(&latest.envelope.manifest_bytes),
        verdict: "invalid".into(),
        reason: Some(reason.into()),
    }
}

fn write_candidate(
    paths: &StatePaths,
    envelope: &SignedManifest,
    before_rename: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    crate::private_storage::open_root(&paths.root)?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_nanos();
    let temporary = paths
        .root
        .join(format!("manifest-sync-{}-{stamp}.tmp", std::process::id()));
    let mut file = crate::private_storage::options()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    std::io::Write::write_all(&mut file, &serde_json::to_vec(envelope)?)?;
    aft_store::sync::sync_all(&file, &temporary)?;
    drop(file);
    before_rename()?;
    fs::rename(&temporary, &paths.manifest)
}

fn activate(paths: &StatePaths, latest: &Latest, now: u64) -> Option<Report> {
    // Refresh the shim's high-water from its current file before comparing.
    let current = local_report(paths, now);
    let acceptance_lock = match manifest_acceptance_lock(paths) {
        Ok(lock) => lock,
        Err(_) => {
            return Some(candidate_report(
                latest,
                "manifest acceptance lock unavailable",
            ))
        }
    };
    if latest.version <= version_high_water(paths) {
        return current;
    }
    let envelope = SignedManifest {
        artifact_id: MANIFEST_ARTIFACT_ID.into(),
        envelope_version: ENVELOPE_VERSION,
        key_id: latest.envelope.key_id.clone(),
        fetched_at_unix_secs: now,
        signature: latest.envelope.signature.clone(),
        manifest_bytes: latest.envelope.manifest_bytes.clone(),
    };
    let verified = match trust_root::read_daemon(paths)
        .and_then(|pin| validate_manifest_candidate(paths, &envelope, now, &pin))
    {
        Ok(verified) => verified,
        Err(problem) => return Some(candidate_report(latest, problem_reason(&problem))),
    };
    if latest.version == 0 || verified.manifest.manifest_version != latest.version {
        return Some(candidate_report(
            latest,
            "manifest version differs from vault version",
        ));
    }
    if latest.version <= version_high_water(paths) {
        return current;
    }
    if write_candidate(paths, &envelope, || Ok(())).is_err() {
        return Some(candidate_report(latest, "manifest atomic write failed"));
    }
    drop(acceptance_lock);
    local_report(paths, now)
}

#[derive(Default)]
struct Cursor {
    epoch: Option<String>,
    seq: u64,
    read_version: Option<u64>,
    read_sha256: Option<String>,
}

impl Cursor {
    fn observe(&mut self, notice: &Notice) -> bool {
        let reread = notice.version != self.read_version
            || self
                .epoch
                .as_ref()
                .is_some_and(|epoch| epoch != &notice.epoch)
            || notice.seq != self.seq.saturating_add(1);
        self.epoch = Some(notice.epoch.clone());
        self.seq = notice.seq;
        reread
    }
}

#[derive(Clone, Copy, Debug)]
struct Failure {
    code: &'static str,
    transient: bool,
}

impl Failure {
    fn new(code: &str, transient: bool) -> Self {
        // Never let arbitrary peer text enlarge health or enter daemon logs.
        let code = match code {
            "unknown_method" => "unknown_method",
            "not_found" => "not_found",
            "home_unavailable" => "home_unavailable",
            "store_error" => "store_error",
            "policy_root_unavailable" => "policy_root_unavailable",
            "remote_setup_incomplete" => "remote_setup_incomplete",
            "secret_setup_incomplete" => "secret_setup_incomplete",
            "invalid_params" => "invalid_params",
            "transport_unavailable" => "transport_unavailable",
            _ => "unrecognized_refusal",
        };
        Self { code, transient }
    }

    fn transport(error: subc_client_rs::CallError) -> Self {
        match error {
            subc_client_rs::CallError::Module(body) => Self::new(&body.code, false),
            _ => Self::new("transport_unavailable", true),
        }
    }

    fn stop(self) -> bool {
        matches!(self.code, "unknown_method" | "not_found")
    }

    fn record(self) {
        HEALTH
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .last_refusal_code = Some(self.code);
    }
}

fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, Failure> {
    let reply: Reply<T> =
        serde_json::from_slice(bytes).map_err(|_| Failure::new("transport_unavailable", true))?;
    match reply.result {
        ResultBody::Success(value) => Ok(value),
        ResultBody::Refused { error } => Err(Failure::new(&error.code, error.class == "transient")),
    }
}

#[derive(Default)]
struct Backoff(u32);

impl Backoff {
    fn delay(&mut self) -> Duration {
        let seconds = 1u64 << self.0.min(6);
        self.0 = self.0.saturating_add(1);
        Duration::from_secs(seconds)
    }
}

async fn report(session: &ManagementSession, report: &Report) -> Result<(), Failure> {
    let mut backoff = Backoff::default();
    loop {
        {
            let mut health = HEALTH
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            health.last_report = Some(report.clone());
            health.last_report_outcome = Some("sending");
        }
        let result = match session
            .request(serde_json::to_vec(&wire::report_request(report)).expect("report JSON"))
            .await
        {
            Ok(bytes) => decode::<Recorded>(&bytes).and_then(|recorded| {
                if recorded.status == "recorded" && recorded.version == report.version {
                    Ok(())
                } else {
                    Err(Failure::new("transport_unavailable", true))
                }
            }),
            Err(error) => Err(Failure::transport(error)),
        };
        {
            let mut health = HEALTH
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            health.last_report_outcome = Some(match result {
                Ok(()) => "recorded",
                Err(failure)
                    if failure.code == "home_unavailable"
                        || failure.code == "transport_unavailable" =>
                {
                    "unknown"
                }
                Err(_) => "refused",
            });
        }
        match result {
            Ok(()) => return Ok(()),
            Err(failure) => {
                failure.record();
                if failure.stop() {
                    return Err(failure);
                }
                if !failure.transient {
                    return Ok(());
                }
                // Reports are idempotent. Keep the exact tuple after an unknown
                // outcome; do not replace it with a newer activation's report.
                tokio::time::sleep(backoff.delay()).await;
                if failure.code == "transport_unavailable" {
                    return Err(failure);
                }
            }
        }
    }
}

async fn latest(session: &ManagementSession) -> Result<Latest, Failure> {
    let bytes = session
        .request(serde_json::to_vec(&wire::latest_request()).expect("latest JSON"))
        .await
        .map_err(Failure::transport)?;
    let latest: Latest = decode(&bytes)?;
    if latest.version == 0 {
        return Err(Failure::new("transport_unavailable", true));
    }
    let mut health = HEALTH
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    health.observe_version(Some(latest.version));
    Ok(latest)
}

async fn refresh(
    session: &ManagementSession,
    paths: &StatePaths,
    root: &RootRefresh<'_>,
    cursor: &mut Cursor,
    notice: Option<&Notice>,
) -> Result<Option<Report>, Failure> {
    let mut fetched = latest(session).await?;
    let disagrees = |fetched: &Latest| {
        notice.is_some_and(|notice| {
            notice.version == Some(fetched.version)
                && notice.manifest_sha256.as_deref()
                    != Some(digest(&fetched.envelope.manifest_bytes).as_str())
        })
    };
    if disagrees(&fetched) {
        fetched = latest(session).await?;
        if disagrees(&fetched) {
            cursor.read_version = Some(fetched.version);
            cursor.read_sha256 = Some(digest(&fetched.envelope.manifest_bytes));
            return Ok(Some(candidate_report(
                &fetched,
                "digest differs from the vault's notice",
            )));
        }
    }
    root.refresh(paths).await?;
    cursor.read_version = Some(fetched.version);
    cursor.read_sha256 = Some(digest(&fetched.envelope.manifest_bytes));
    Ok(activate(paths, &fetched, unix_seconds()))
}

async fn run_with<F, Fut>(paths: &StatePaths, root: &RootRefresh<'_>, mut connect: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<ManagementSession, ()>>,
{
    let mut backoff = Backoff::default();
    let mut pending_report = None;
    loop {
        let session = match tokio::time::timeout(Duration::from_secs(15), connect()).await {
            Ok(Ok(session)) => session,
            _ => {
                Failure::new("transport_unavailable", true).record();
                tokio::time::sleep(backoff.delay()).await;
                continue;
            }
        };
        let mut cursor = Cursor::default();
        let result: Result<(), Failure> = async {
            if let Some(pending) = pending_report.as_ref() {
                report(&session, pending).await?;
                pending_report = None;
            }
            let first = refresh(&session, paths, root, &mut cursor, None).await;
            if let Err(failure) = first {
                if failure.stop() {
                    if let Some(current) = local_report(paths, unix_seconds()) {
                        let _ = report(&session, &current).await;
                    }
                }
                return Err(failure);
            }
            pending_report = first?;
            if let Some(pending) = pending_report.as_ref() {
                report(&session, pending).await?;
                pending_report = None;
            }
            let mut subscription = session
                .watch(serde_json::to_vec(&wire::watch_request()).expect("watch JSON"))
                .await
                .map_err(Failure::transport)?;
            loop {
                let Some(bytes) = subscription.events().recv().await else {
                    // The SDK closes this receiver after a normal STREAM_END
                    // frame or a lost watch. Reconnect always reads latest anew.
                    return match subscription.closed().await {
                        Ok(_) => Err(Failure::new("transport_unavailable", true)),
                        Err(error) => Err(Failure::transport(error)),
                    };
                };
                if let Ok(reply) = serde_json::from_slice::<Reply<Value>>(&bytes) {
                    if let ResultBody::Refused { error } = reply.result {
                        return Err(Failure::new(&error.code, error.class == "transient"));
                    }
                }
                let notice: Notice = serde_json::from_slice(&bytes)
                    .map_err(|_| Failure::new("transport_unavailable", true))?;
                if notice.kind != "changed"
                    || notice.seq == 0
                    || notice.version.is_some() != notice.manifest_sha256.is_some()
                {
                    return Err(Failure::new("transport_unavailable", true));
                }
                HEALTH
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .observe_version(notice.version);
                backoff = Backoff::default();
                let hash_differs = notice.version == cursor.read_version
                    && notice.manifest_sha256 != cursor.read_sha256;
                if cursor.observe(&notice) || hash_differs {
                    pending_report =
                        refresh(&session, paths, root, &mut cursor, Some(&notice)).await?;
                    if let Some(pending) = pending_report.as_ref() {
                        report(&session, pending).await?;
                        pending_report = None;
                    }
                }
            }
        }
        .await;
        session.close().await;
        if let Err(failure) = result {
            failure.record();
            if failure.stop() {
                log::info!("manifest sync stopped: {}", failure.code);
                return;
            }
        }
        tokio::time::sleep(backoff.delay()).await;
    }
}

/// Keeps delivery alive for exactly one attached module session.
#[doc(hidden)]
pub struct Worker {
    _stop: tokio::sync::oneshot::Sender<()>,
}

/// Called only by subc startup after registration, never by a standalone shim.
#[doc(hidden)]
pub fn start(
    connection_file: PathBuf,
    read_root: impl Fn() -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<trust_root::Reply, ()>> + Send>,
        > + Send
        + Sync
        + 'static,
) -> Option<Worker> {
    if !aft_base::registration::module_consumer_allowed()
        || aft_base::registration::module_id().as_deref() != Some("aft")
    {
        return None;
    }
    let (stop, stopped) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        if enable_gate::gh_shim_enabled_from_config_doc(&read_user_config_doc().unwrap_or_default())
            == Some(false)
        {
            return;
        }
        if let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            let paths = StatePaths::from_process().for_daemon();
            let root = RootRefresh::new(&read_root);
            runtime.block_on(async {
                tokio::select! {
                    _ = stopped => {},
                    _ = async {
                        tokio::join!(root.poll(&paths), run_with(&paths, &root, || ManagementSession::connect(&connection_file)));
                    } => {},
                }
            });
        }
    });
    Some(Worker { _stop: stop })
}

#[cfg(test)]
#[path = "manifest_sync_tests.rs"]
mod tests;
