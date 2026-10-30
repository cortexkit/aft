//! Launch-only secret state. Credentials are attached to a prepared task in
//! memory and applied at exec, never to its persisted environment snapshot.

use std::collections::HashMap;
#[cfg(any(unix, test))]
use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;
use std::sync::Arc;
#[cfg(any(unix, test))]
use std::sync::Mutex;

#[cfg(any(unix, test))]
use crate::context::AppContext;
use crate::protocol::Response;
#[cfg(any(unix, test))]
use crate::sandbox_spawn::SpawnPlan;
#[cfg(unix)]
use crate::sandbox_spawn::{RequestedSandboxTier, SandboxTaskKind};
use crate::secret::{SecretVault, UnavailableVault};
#[cfg(unix)]
use crate::secret_proxy::{GrantedLabel, ProxyConfig, SecretProxy};

pub(crate) const REMOTE_REFUSED: &str =
    "Secret placeholders stay local; remote commands cannot use them.";
pub(crate) const SANDBOX_REQUIRED: &str = "Secret placeholder commands require a protected sandbox on Linux or macOS; Windows cannot run them.";
pub(crate) const ENV_CONFLICT: &str =
    "Secret placeholder commands cannot set proxy variables in env or bash.env.";
#[cfg(unix)]
pub(crate) const PROXY_UNAVAILABLE: &str =
    "The secret proxy or its trust bundle could not be prepared; no command was started.";

pub(crate) const PROXY_KEYS: [&str; 4] = ["HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy"];
pub(crate) const REMOVE_KEYS: [&str; 4] = ["NO_PROXY", "no_proxy", "ALL_PROXY", "all_proxy"];
#[cfg(unix)]
pub(crate) const TRUST_KEYS: [&str; 5] = [
    "SSL_CERT_FILE",
    "CURL_CA_BUNDLE",
    "NODE_EXTRA_CA_CERTS",
    "REQUESTS_CA_BUNDLE",
    "GIT_SSL_CAINFO",
];

pub(crate) fn labels(command: &str) -> Vec<String> {
    let mut labels = Vec::new();
    for placeholder in secret_rules::placeholder::scan(command) {
        if !labels.iter().any(|label| label == placeholder.label) {
            labels.push(placeholder.label.to_owned());
        }
    }
    labels
}

pub(crate) fn env_conflicts(env: &HashMap<String, String>) -> bool {
    PROXY_KEYS
        .iter()
        .chain(REMOVE_KEYS.iter())
        .any(|key| env.contains_key(*key))
}

pub(crate) fn vault() -> Arc<dyn SecretVault> {
    #[cfg(test)]
    if let Some(vault) = TEST_VAULT.with(|slot| slot.borrow().clone()) {
        return vault;
    }
    Arc::new(UnavailableVault)
}

pub(crate) fn refusal(id: &str, labels: &[String], code: &str, message: &str) -> Response {
    let vault = vault();
    let scope = crate::subc::current_secret_audit_scope().map(|scope| scope.to_scope_ref());
    for label in labels {
        let mut record = secret_rules::audit::AuditRecordV1::new(
            crate::secret::now_ms(),
            label,
            secret_rules::audit::Decision::Refused,
            secret_rules::audit::AuditReason::Refused,
            id,
            id,
        );
        record.scope = scope.clone();
        let _ = vault.audit(&record);
    }
    Response::error(id, code, message)
}

#[cfg(unix)]
pub(crate) struct Preflight {
    pub(crate) plan: SpawnPlan,
    pub(crate) task: Option<super::persistence::ResolvedTask>,
}

#[cfg(unix)]
impl Drop for Preflight {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            self.plan.cleanup_unspawned();
            let _ = super::persistence::delete_resolved_task(&task);
        }
    }
}

#[cfg(unix)]
pub(crate) fn preflight(
    ctx: &AppContext,
    session: &str,
    pty: bool,
    background: bool,
) -> Result<Preflight, ()> {
    let task = super::persistence::allocate_task_layout(&super::task_storage_dir(ctx), session)
        .map_err(|error| {
            #[cfg(test)]
            eprintln!("secret preflight layout refused: {error}");
            #[cfg(not(test))]
            let _ = error;
        })?;
    let principal = crate::sandbox_spawn::current_authenticated_principal();
    let plan = crate::sandbox_spawn::resolve_sandbox_spawn(
        ctx,
        &principal,
        RequestedSandboxTier::Native,
        if pty {
            SandboxTaskKind::BashPty
        } else if background {
            SandboxTaskKind::BashBackground
        } else {
            SandboxTaskKind::BashForeground
        },
        &task.paths.io_dir,
        None,
    );
    let preflight = Preflight {
        plan,
        task: Some(task),
    };
    if !preflight.plan.protects_secret_memory() {
        #[cfg(test)]
        eprintln!(
            "secret preflight refused: code={:?}, message={:?}, scratch={:?}",
            preflight.plan.refusal_code(),
            preflight.plan.refusal_message(),
            preflight.plan.temp_dir()
        );
        return Err(());
    }
    Ok(preflight)
}

#[cfg(unix)]
pub(crate) type SharedRuntime = Arc<Mutex<Runtime>>;

#[cfg(unix)]
pub(crate) struct Runtime {
    proxy: SecretProxy,
    bundle: PathBuf,
    pub(crate) lifetime: crate::secret::SecretScope,
    pub(crate) authorization: crate::secret::SecretScope,
    armed: bool,
}

#[cfg(unix)]
impl Runtime {
    #[cfg(test)]
    pub(crate) fn address(&self) -> std::net::SocketAddr {
        self.proxy.address()
    }
    pub(crate) fn arm(&mut self, pid: u32) -> Result<(), String> {
        self.proxy.arm_command(pid).map_err(|reason| {
            #[cfg(test)]
            eprintln!("secret proxy arming failed: {reason:?}");
            #[cfg(not(test))]
            let _ = reason;
            PROXY_UNAVAILABLE.to_owned()
        })?;
        self.armed = true;
        Ok(())
    }

    pub(crate) fn stop(&mut self) {
        self.proxy.stop();
        let _ = std::fs::remove_file(&self.bundle);
    }

    pub(crate) fn exec_environment(&self) -> Vec<(String, String)> {
        let url = self.proxy.proxy_url();
        PROXY_KEYS
            .iter()
            .map(|key| ((*key).to_owned(), url.to_string()))
            .chain(TRUST_KEYS.iter().map(|key| {
                (
                    (*key).to_owned(),
                    self.bundle.to_string_lossy().into_owned(),
                )
            }))
            .collect()
    }
}

#[cfg(unix)]
impl Drop for Runtime {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(unix)]
pub(crate) struct Launch {
    pub(crate) preflight: Preflight,
    pub(crate) runtime: SharedRuntime,
    adopted: bool,
}

#[cfg(unix)]
impl Launch {
    pub(crate) fn spawned(&mut self) {
        self.adopted = self.runtime.lock().unwrap_or_else(|p| p.into_inner()).armed;
    }
}

#[cfg(unix)]
impl Drop for Launch {
    fn drop(&mut self) {
        let mut runtime = self.runtime.lock().unwrap_or_else(|p| p.into_inner());
        if !self.adopted {
            // Prepared payloads can retain runtime references after a failed
            // launch. Only a successfully registered, armed spawn transfers
            // ownership; arming before a later startup failure is not enough.
            runtime.stop();
            self.preflight.plan.cleanup_unspawned();
        }
    }
}

#[cfg(unix)]
pub(crate) fn authorize(
    id: &str,
    labels: &[String],
    mut preflight: Preflight,
    env: &HashMap<String, String>,
) -> Result<Launch, Response> {
    if env_conflicts(env) {
        return Err(refusal(id, labels, "secrets_env_conflict", ENV_CONFLICT));
    }
    let (scopes, _) = crate::subc::current_call_scopes().map_err(|reason| {
        outcome_response(
            id,
            labels,
            crate::secret::refuse_ineligible(&labels[0], reason),
        )
    })?;
    let vault = vault();
    let mut grants = Vec::new();
    for label in labels {
        match crate::subc::resolve_current_secret_label(vault.as_ref(), label) {
            crate::subc::LaunchDecision::Proceed {
                mut rule,
                grant_id,
                expires_at_ms,
            } => {
                rule.grant_id = grant_id;
                grants.push(GrantedLabel {
                    rule,
                    expires_at_ms,
                });
            }
            crate::subc::LaunchDecision::Refuse(outcome) => {
                return Err(outcome_response(id, labels, outcome))
            }
            crate::subc::LaunchDecision::Pending(pending) => {
                return Err(refusal(
                    id,
                    labels,
                    crate::secret::SECRET_ACCESS_PENDING,
                    pending.message,
                ))
            }
        }
    }
    let task = preflight.task.as_ref().expect("preflight owns layout");
    let proxy = start_proxy(ProxyConfig {
        vault,
        authorization_scope: scopes.authorization.clone(),
        lifetime_scope: scopes.lifetime.clone(),
        scanned_labels: labels.to_vec(),
        grants,
        origin: task.paths.task_id.clone(),
    })
    .map_err(|_| refusal(id, labels, "secrets_unavailable", PROXY_UNAVAILABLE))?;
    preflight
        .plan
        .enable_secret_proxy(proxy.address().port())
        .map_err(|_| refusal(id, labels, "secrets_sandbox_required", SANDBOX_REQUIRED))?;
    let bundle = preflight
        .plan
        .temp_dir()
        .ok_or_else(|| refusal(id, labels, "secrets_sandbox_required", SANDBOX_REQUIRED))?
        .join("secret-trust.pem");
    write_bundle(&bundle, proxy.ca_pem())
        .map_err(|_| refusal(id, labels, "secrets_unavailable", PROXY_UNAVAILABLE))?;
    Ok(Launch {
        preflight,
        adopted: false,
        runtime: Arc::new(Mutex::new(Runtime {
            proxy,
            bundle,
            lifetime: scopes.lifetime,
            authorization: scopes.authorization,
            armed: false,
        })),
    })
}

#[cfg(all(not(test), unix))]
fn start_proxy(config: ProxyConfig) -> Result<SecretProxy, crate::secret_proxy::ProxyError> {
    SecretProxy::start(config)
}

// Only test builds can inject this upstream config. Release launches always
// call SecretProxy::start; neither a tool argument nor a test-only switch can
// select this override.
#[cfg(all(test, unix))]
fn start_proxy(config: ProxyConfig) -> Result<SecretProxy, crate::secret_proxy::ProxyError> {
    match TEST_UPSTREAM.with(|slot| slot.borrow().clone()) {
        Some(upstream) => SecretProxy::start_for_test(config, upstream),
        None => SecretProxy::start(config),
    }
}

#[cfg(unix)]
fn outcome_response(id: &str, labels: &[String], outcome: crate::secret::LabelOutcome) -> Response {
    match outcome {
        crate::secret::LabelOutcome::Refuse { code, message } => {
            refusal(id, labels, code, &message)
        }
        _ => refusal(id, labels, "secrets_unavailable", PROXY_UNAVAILABLE),
    }
}

#[cfg(unix)]
fn write_bundle(path: &Path, ca: &str) -> Result<(), ()> {
    let mut pem = Vec::new();
    let native = zeroize::Zeroizing::new(native_roots()?);
    append_public_certificates(&mut pem, &native)?;
    for key in TRUST_KEYS {
        // Only daemon-inherited trust paths are authority to read a file.
        // Tool env values are replaced, not read on the caller's behalf.
        if let Some(path) = std::env::var_os(key).map(PathBuf::from) {
            let input = zeroize::Zeroizing::new(std::fs::read(path).map_err(|_| ())?);
            append_public_certificates(&mut pem, &input)?;
        }
    }
    append_public_certificates(&mut pem, ca.as_bytes())?;
    crate::private_storage::write(path, pem).map_err(|_| ())
}

#[cfg(unix)]
fn append_public_certificates(output: &mut Vec<u8>, input: &[u8]) -> Result<(), ()> {
    use base64::Engine;
    use rustls::pki_types::pem::PemObject;
    let mut count = 0;
    for certificate in rustls::pki_types::CertificateDer::pem_slice_iter(input) {
        let certificate = certificate.map_err(|_| ())?;
        // Extracting a trust anchor validates certificate structure without
        // treating private-key blocks or arbitrary PEM text as public data.
        rustls::RootCertStore::empty()
            .add(certificate.clone())
            .map_err(|_| ())?;
        output.extend_from_slice(b"-----BEGIN CERTIFICATE-----\n");
        let encoded = base64::engine::general_purpose::STANDARD.encode(certificate.as_ref());
        for line in encoded.as_bytes().chunks(64) {
            output.extend_from_slice(line);
            output.push(b'\n');
        }
        output.extend_from_slice(b"-----END CERTIFICATE-----\n");
        count += 1;
    }
    if count == 0 {
        Err(())
    } else {
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn native_roots() -> Result<Vec<u8>, ()> {
    for path in [
        "/etc/ssl/certs/ca-certificates.crt",
        "/etc/pki/tls/certs/ca-bundle.crt",
        "/etc/ssl/ca-bundle.pem",
        "/etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem",
        "/etc/ssl/cert.pem",
    ] {
        if let Ok(pem) = std::fs::read(path) {
            if !pem.is_empty() {
                return Ok(pem);
            }
        }
    }
    Err(())
}

#[cfg(target_os = "macos")]
fn native_roots() -> Result<Vec<u8>, ()> {
    let mut pem = Vec::new();
    for keychain in [
        "/System/Library/Keychains/SystemRootCertificates.keychain",
        "/Library/Keychains/System.keychain",
    ] {
        let output = std::process::Command::new("/usr/bin/security")
            .args(["find-certificate", "-a", "-p", keychain])
            .output()
            .map_err(|_| ())?;
        if !output.status.success() {
            return Err(());
        }
        pem.extend(output.stdout);
        pem.push(b'\n');
    }
    if pem.is_empty() {
        return Err(());
    }
    Ok(pem)
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn native_roots() -> Result<Vec<u8>, ()> {
    Err(())
}

#[cfg(test)]
thread_local! {
    static TEST_VAULT: std::cell::RefCell<Option<Arc<dyn SecretVault>>> = const { std::cell::RefCell::new(None) };
    #[cfg(unix)]
    static TEST_UPSTREAM: std::cell::RefCell<Option<rustls::ClientConfig>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
#[path = "secrets_tests.rs"]
mod tests;
