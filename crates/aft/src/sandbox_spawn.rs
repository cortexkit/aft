//! Policy and process wiring for agent-provided shell commands.
//!
//! Every agent bash process reaches [`resolve_sandbox_spawn`] and carries the
//! resulting [`SpawnPlan`] into one of the two process-creation primitives:
//! detached pipes or PTY. Foreground orchestration uses the detached registry
//! too, so it does not create a third process-creation path.
//!
//! AFT also starts processes for its own implementation. Those are outside this
//! seam because they do not execute an agent command: external formatters,
//! linters, and type checkers in `format`; LSP servers and Windows LSP cleanup in
//! `lsp::client` and `lsp::child_registry`; git probes in `search_index`,
//! `readonly_artifacts`, `commands::configure`, and `commands::conflicts`; login
//! shell PATH discovery in `effective_path`; and Windows process liveness or
//! termination helpers in `artifact_owner`, `fs_lock`, and
//! `bash_background::process`. Image and PDF handling in `commands::read` is
//! in-process and creates no child. Keeping this inventory here makes the
//! agent-command boundary explicit without accidentally applying agent policy to
//! AFT's internal tooling.

use std::cell::RefCell;
#[cfg(any(test, target_os = "linux"))]
use std::collections::BTreeSet;
use std::collections::{BTreeMap, HashMap};
#[cfg(target_os = "linux")]
use std::ffi::{CStr, CString};
use std::ffi::{OsStr, OsString};
#[cfg(unix)]
use std::fs::DirBuilder;
use std::fs::File;
#[cfg(unix)]
use std::io::{Read, Seek, SeekFrom};
#[cfg(unix)]
use std::os::fd::RawFd;
#[cfg(target_os = "linux")]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
#[cfg(target_os = "linux")]
use std::os::unix::ffi::OsStrExt;
#[cfg(not(unix))]
type RawFd = i32;
#[cfg(unix)]
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::Command;
#[cfg(unix)]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(unix)]
use std::sync::Arc;
use std::sync::{Mutex, OnceLock};
#[cfg(unix)]
use std::time::{Duration, Instant};

use portable_pty::CommandBuilder;

use crate::context::AppContext;
use crate::sandbox_profile::SandboxProfile;

pub const SANDBOX_UNAVAILABLE_EXIT_CODE: i32 = 78;

/// Server-authenticated trust classification for a route bind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrincipalTrust {
    FirstParty,
    Untrusted,
}

/// Principal data supplied by the server-side transport, never by a bash body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthenticatedPrincipal {
    /// Standalone NDJSON and first-party plugin bindings have no route identity.
    FirstParty,
    /// Identity captured from an authenticated subc route bind.
    RouteBind {
        trust: PrincipalTrust,
        route_channel: u16,
        route_epoch: u32,
        project_root: PathBuf,
        harness: String,
        session_id: String,
        /// Server principal label (`direct`, `reserved:<module>`, or
        /// `unverified`). `None` preserves an absent principal for future
        /// fail-closed policy instead of silently treating it as first-party.
        principal_id: Option<String>,
    },
}

/// Sandbox tier requested by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestedSandboxTier {
    Disabled,
    Native,
    Host,
}

pub(crate) type ChildEnvironment = BTreeMap<OsString, OsString>;

#[cfg(unix)]
const ESCALATION_GRANT_TTL: Duration = Duration::from_secs(120);
#[cfg(unix)]
const ESCALATION_DIGEST_TAG: &[u8] = b"aft-escalation-payload-v3";
#[cfg(unix)]
const ENVIRONMENT_TAG: &[u8] = b"AFTENV1\0";
#[cfg(unix)]
const ESCALATION_TIER: &[u8] = b"host";

#[cfg(unix)]
#[derive(Debug, Clone)]
struct EscalationGrant {
    principal: AuthenticatedPrincipal,
    root: PathBuf,
    digest: blake3::Hash,
    expires_at: Instant,
    consumed: bool,
    session_dir: PathBuf,
    task_id: String,
}

#[cfg(unix)]
#[derive(Debug, Default)]
pub(crate) struct EscalationGrantStore {
    grants: HashMap<String, EscalationGrant>,
}

#[cfg(unix)]
impl EscalationGrantStore {
    #[cfg(test)]
    pub(crate) fn len_for_test(&self) -> usize {
        self.grants.len()
    }
}

#[derive(Debug, Clone)]
pub struct HostEscalationAttempt {
    pub grant_id: String,
    pub command: Vec<u8>,
    pub root: PathBuf,
    pub cwd: PathBuf,
    pub shell_path: PathBuf,
    pub environment: ChildEnvironment,
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EscalationRefusal {
    Expired,
    Consumed,
    DigestMismatch,
    WrongPrincipal,
}

#[cfg(unix)]
impl EscalationRefusal {
    pub(crate) fn class(self) -> &'static str {
        match self {
            Self::Expired => "expired",
            Self::Consumed => "consumed",
            Self::DigestMismatch => "digest_mismatch",
            Self::WrongPrincipal => "wrong_principal",
        }
    }
}

/// Agent-command path that is about to create a process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxTaskKind {
    BashForeground,
    BashBackground,
    BashPty,
}

#[cfg(unix)]
struct PreparedTaskInner {
    paths: crate::bash_background::persistence::TaskPaths,
    dirs: crate::bash_background::persistence::TaskDirs,
    digest: blake3::Hash,
    environment: ChildEnvironment,
    command_bytes: Arc<Vec<u8>>,
    wrapper_bytes: Arc<Vec<u8>>,
    _command_file: Arc<File>,
    _wrapper_file: Arc<File>,
    _environment_file: Arc<File>,
}

/// A materialized payload whose bytes have already been validated through held handles.
#[cfg(unix)]
#[derive(Clone)]
#[doc(hidden)]
pub struct PreparedTask(Arc<PreparedTaskInner>);

#[cfg(unix)]
impl std::fmt::Debug for PreparedTask {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedTask")
            .field("task_id", &self.0.paths.task_id)
            .field("digest", &self.0.digest.to_hex().as_str())
            .finish_non_exhaustive()
    }
}

#[cfg(unix)]
impl PartialEq for PreparedTask {
    fn eq(&self, other: &Self) -> bool {
        self.0.paths.task_id == other.0.paths.task_id
            && self.0.paths.session_dir == other.0.paths.session_dir
            && self.0.digest == other.0.digest
    }
}

#[cfg(unix)]
impl Eq for PreparedTask {}

#[cfg(unix)]
pub(crate) struct PayloadInvocation {
    pub(crate) wrapper_text: OsString,
    pub(crate) command_text: OsString,
}

#[cfg(unix)]
impl PreparedTask {
    #[cfg(test)]
    pub(crate) fn paths(&self) -> &crate::bash_background::persistence::TaskPaths {
        &self.0.paths
    }

    pub(crate) fn resolved_task(&self) -> crate::bash_background::persistence::ResolvedTask {
        crate::bash_background::persistence::ResolvedTask {
            paths: self.0.paths.clone(),
            dirs: self.0.dirs.clone(),
        }
    }

    pub(crate) fn environment(&self) -> &ChildEnvironment {
        &self.0.environment
    }

    pub(crate) fn command_text(&self) -> Result<&str, String> {
        std::str::from_utf8(&self.0.command_bytes)
            .map_err(|error| format!("verified bash command is not UTF-8: {error}"))
    }

    pub(crate) fn payload_read_grants(&self) -> Vec<PathBuf> {
        control_payload_read_grants(&self.0.paths.io_dir)
            .expect("prepared task paths already passed strict validation")
    }

    pub(crate) fn invocation(&self) -> Result<PayloadInvocation, String> {
        let wrapper = std::str::from_utf8(&self.0.wrapper_bytes)
            .map_err(|error| format!("verified wrapper payload is not UTF-8: {error}"))?;
        Ok(PayloadInvocation {
            wrapper_text: OsString::from(wrapper),
            command_text: OsString::from(self.command_text()?),
        })
    }
}

/// Complete process-launch decision consumed by a spawn primitive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpawnPlan {
    Unsandboxed,
    Host {
        shell_path: PathBuf,
        environment: ChildEnvironment,
    },
    Launcher {
        profile: SandboxProfile,
        launcher_path: PathBuf,
    },
    #[cfg(unix)]
    Prepared {
        plan: Box<SpawnPlan>,
        task: PreparedTask,
    },
    Refused {
        code: &'static str,
        message: String,
        mismatch_class: Option<&'static str>,
    },
}

impl SpawnPlan {
    fn policy(&self) -> &Self {
        #[cfg(unix)]
        if let Self::Prepared { plan, .. } = self {
            return plan.policy();
        }
        self
    }

    #[cfg(unix)]
    pub(crate) fn with_prepared_task(self, task: PreparedTask) -> Self {
        if matches!(self, Self::Refused { .. }) {
            return self;
        }

        #[cfg(target_os = "linux")]
        let mut plan = self;
        #[cfg(not(target_os = "linux"))]
        let plan = self;

        #[cfg(target_os = "linux")]
        if let Self::Launcher { profile, .. } = &mut plan {
            let payload_grants = task.payload_read_grants();
            if let Err(error) = add_linux_payload_read_grants(profile, &payload_grants) {
                return Self::Refused {
                    code: "sandbox_unavailable",
                    message: format!(
                        "native sandbox payload read grants violate the read floor: {error}"
                    ),
                    mismatch_class: None,
                };
            }
        }

        Self::Prepared {
            plan: Box::new(plan),
            task,
        }
    }

    #[cfg(unix)]
    pub(crate) fn prepared_task(&self) -> Option<&PreparedTask> {
        match self {
            Self::Prepared { task, .. } => Some(task),
            _ => None,
        }
    }

    pub fn payload_read_grants(&self) -> Vec<PathBuf> {
        #[cfg(unix)]
        if let Some(task) = self.prepared_task() {
            return task.payload_read_grants();
        }
        Vec::new()
    }

    pub(crate) fn refusal_code(&self) -> Option<&'static str> {
        match self.policy() {
            Self::Refused { code, .. } => Some(code),
            _ => None,
        }
    }

    pub(crate) fn refusal_message(&self) -> Option<&str> {
        match self.policy() {
            Self::Refused { message, .. } => Some(message),
            _ => None,
        }
    }

    pub(crate) fn refusal_mismatch_class(&self) -> Option<&'static str> {
        match self.policy() {
            Self::Refused { mismatch_class, .. } => *mismatch_class,
            _ => None,
        }
    }

    pub(crate) fn is_native_launcher(&self) -> bool {
        matches!(self.policy(), Self::Launcher { .. })
    }

    #[cfg(unix)]
    pub(crate) fn host_shell_path(&self) -> Option<&Path> {
        match self.policy() {
            Self::Host { shell_path, .. } => Some(shell_path),
            _ => None,
        }
    }

    pub(crate) fn temp_dir(&self) -> Option<&Path> {
        match self.policy() {
            Self::Launcher { profile, .. } => Some(&profile.temp_dir),
            _ => None,
        }
    }

    pub(crate) fn cleanup_unspawned(&self) {
        let Some(temp_dir) = self.temp_dir() else {
            return;
        };
        if is_managed_task_temp_dir(temp_dir) {
            let _ = std::fs::remove_dir_all(temp_dir);
        }
    }

    #[cfg(test)]
    #[cfg(unix)]
    pub(crate) fn launcher_for_test(profile: SandboxProfile, launcher_path: PathBuf) -> Self {
        Self::Launcher {
            profile,
            launcher_path,
        }
    }

    #[cfg(test)]
    pub(crate) fn refused_for_test(code: &'static str) -> Self {
        Self::Refused {
            code,
            message: format!("bash process creation refused by sandbox policy: {code}"),
            mismatch_class: None,
        }
    }
}

/// One resolver invocation captured by the project-keyed test seam.
#[doc(hidden)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxSpawnObservation {
    pub principal: AuthenticatedPrincipal,
    pub requested_tier: RequestedSandboxTier,
    pub task_kind: SandboxTaskKind,
}

static TEST_OBSERVATIONS: OnceLock<Mutex<HashMap<PathBuf, Vec<SandboxSpawnObservation>>>> =
    OnceLock::new();

thread_local! {
    static CURRENT_PRINCIPAL: RefCell<Option<AuthenticatedPrincipal>> = const { RefCell::new(None) };
    #[cfg(test)]
    static TEST_PLAN_OVERRIDE: RefCell<Option<SpawnPlan>> = const { RefCell::new(None) };
}

struct PrincipalScope(Option<AuthenticatedPrincipal>);

impl Drop for PrincipalScope {
    fn drop(&mut self) {
        CURRENT_PRINCIPAL.with(|slot| {
            slot.replace(self.0.take());
        });
    }
}

/// Run dispatch with server-owned principal data installed for bash resolution.
pub(crate) fn with_authenticated_principal<R>(
    principal: AuthenticatedPrincipal,
    run: impl FnOnce() -> R,
) -> R {
    let previous = CURRENT_PRINCIPAL.with(|slot| slot.replace(Some(principal)));
    let _scope = PrincipalScope(previous);
    run()
}

/// Current dispatch principal. Standalone requests are first-party by construction.
pub(crate) fn current_authenticated_principal() -> AuthenticatedPrincipal {
    CURRENT_PRINCIPAL
        .with(|slot| slot.borrow().clone())
        .unwrap_or(AuthenticatedPrincipal::FirstParty)
}

pub(crate) fn principal_is_first_party(principal: &AuthenticatedPrincipal) -> bool {
    matches!(
        principal,
        AuthenticatedPrincipal::FirstParty
            | AuthenticatedPrincipal::RouteBind {
                trust: PrincipalTrust::FirstParty,
                ..
            }
    )
}

#[cfg(unix)]
pub(crate) fn approved_payload_environment(
    overrides: &HashMap<String, String>,
    temp_dir: &Path,
) -> ChildEnvironment {
    sandboxed_child_environment(overrides, temp_dir)
}

#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn mint_host_escalation_grant(
    ctx: &AppContext,
    principal: &AuthenticatedPrincipal,
    command: &[u8],
    root: &Path,
    cwd: &Path,
    shell_path: &Path,
    environment: &ChildEnvironment,
    storage_dir: &Path,
    session_id: &str,
) -> Result<String, String> {
    mint_host_escalation_grant_at(
        ctx,
        principal,
        command,
        root,
        cwd,
        shell_path,
        environment,
        storage_dir,
        session_id,
        Instant::now(),
    )
}

#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
fn mint_host_escalation_grant_at(
    ctx: &AppContext,
    principal: &AuthenticatedPrincipal,
    command: &[u8],
    root: &Path,
    cwd: &Path,
    shell_path: &Path,
    environment: &ChildEnvironment,
    storage_dir: &Path,
    session_id: &str,
    now: Instant,
) -> Result<String, String> {
    let task = crate::bash_background::persistence::allocate_task_layout(storage_dir, session_id)
        .map_err(|error| format!("failed to allocate escalation payload bundle: {error}"))?;
    let prepared = match prepare_task_payload(
        &task,
        command,
        root,
        cwd,
        principal,
        shell_path,
        environment,
    ) {
        Ok(prepared) => prepared,
        Err(error) => {
            let _ = crate::bash_background::persistence::delete_resolved_task(&task);
            return Err(error);
        }
    };
    let mut store = ctx.escalation_grants().lock();
    let grant_id = loop {
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random)
            .map_err(|error| format!("failed to mint sandbox escalation grant: {error}"))?;
        let candidate = format!("esc_{}", hex_bytes(&random));
        if !store.grants.contains_key(&candidate) {
            break candidate;
        }
    };
    let grant = EscalationGrant {
        principal: principal.clone(),
        root: root.to_path_buf(),
        digest: prepared.0.digest,
        expires_at: now + ESCALATION_GRANT_TTL,
        consumed: false,
        session_dir: prepared.0.paths.session_dir.clone(),
        task_id: prepared.0.paths.task_id.clone(),
    };
    store.grants.insert(grant_id.clone(), grant);
    Ok(grant_id)
}

#[cfg(unix)]
fn consume_host_escalation_grant_at(
    ctx: &AppContext,
    principal: &AuthenticatedPrincipal,
    attempt: &HostEscalationAttempt,
    now: Instant,
) -> Result<PreparedTask, EscalationRefusal> {
    let (digest, session_dir, task_id) = {
        let mut store = ctx.escalation_grants().lock();
        let Some(grant) = store.grants.get_mut(&attempt.grant_id) else {
            return Err(EscalationRefusal::DigestMismatch);
        };
        if grant.consumed {
            return Err(EscalationRefusal::Consumed);
        }
        if now >= grant.expires_at {
            grant.consumed = true;
            return Err(EscalationRefusal::Expired);
        }
        if grant.principal != *principal {
            grant.consumed = true;
            return Err(EscalationRefusal::WrongPrincipal);
        }
        if grant.root != attempt.root {
            grant.consumed = true;
            return Err(EscalationRefusal::DigestMismatch);
        }
        grant.consumed = true;
        (
            grant.digest,
            grant.session_dir.clone(),
            grant.task_id.clone(),
        )
    };

    let task = crate::bash_background::persistence::resolve_uninitialized_task_layout(
        &session_dir,
        &task_id,
    )
    .map_err(|_| EscalationRefusal::DigestMismatch)?;
    verify_payload(
        task,
        &attempt.command,
        &attempt.root,
        &attempt.cwd,
        principal,
        &attempt.shell_path,
        &attempt.environment,
        Some(digest),
        true,
    )
    .map_err(|_| EscalationRefusal::DigestMismatch)
}

#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare_task_payload(
    task: &crate::bash_background::persistence::ResolvedTask,
    command: &[u8],
    root: &Path,
    cwd: &Path,
    principal: &AuthenticatedPrincipal,
    shell_path: &Path,
    environment: &ChildEnvironment,
) -> Result<PreparedTask, String> {
    materialize_payload(
        crate::bash_background::persistence::ResolvedTask {
            paths: task.paths.clone(),
            dirs: task.dirs.clone(),
        },
        command,
        root,
        cwd,
        principal,
        shell_path,
        environment,
    )
}

#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
fn materialize_payload(
    task: crate::bash_background::persistence::ResolvedTask,
    command_bytes: &[u8],
    root: &Path,
    cwd: &Path,
    principal: &AuthenticatedPrincipal,
    shell_path: &Path,
    environment: &ChildEnvironment,
) -> Result<PreparedTask, String> {
    let environment_bytes = encode_environment(environment);
    let digest = payload_digest(
        &task.paths.task_id,
        command_bytes,
        crate::bash_background::process::PAYLOAD_WRAPPER,
        &environment_bytes,
        root,
        cwd,
        principal,
        shell_path,
        environment,
    );
    crate::bash_background::persistence::create_control_file(
        &task.dirs,
        crate::bash_background::persistence::COMMAND_FILE,
        command_bytes,
    )
    .map_err(|error| format!("failed to materialize command payload: {error}"))?;
    crate::bash_background::persistence::create_control_file(
        &task.dirs,
        crate::bash_background::persistence::WRAPPER_FILE,
        crate::bash_background::process::PAYLOAD_WRAPPER,
    )
    .map_err(|error| format!("failed to materialize wrapper payload: {error}"))?;
    crate::bash_background::persistence::create_control_file(
        &task.dirs,
        crate::bash_background::persistence::ENVIRONMENT_FILE,
        &environment_bytes,
    )
    .map_err(|error| format!("failed to materialize environment payload: {error}"))?;
    crate::bash_background::persistence::create_control_file(
        &task.dirs,
        crate::bash_background::persistence::MANIFEST_FILE,
        digest.as_bytes(),
    )
    .map_err(|error| format!("failed to materialize payload manifest: {error}"))?;
    verify_payload(
        task,
        command_bytes,
        root,
        cwd,
        principal,
        shell_path,
        environment,
        Some(digest),
        true,
    )
}

#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
fn verify_payload(
    task: crate::bash_background::persistence::ResolvedTask,
    expected_command: &[u8],
    root: &Path,
    cwd: &Path,
    principal: &AuthenticatedPrincipal,
    shell_path: &Path,
    expected_environment: &ChildEnvironment,
    expected_digest: Option<blake3::Hash>,
    reject_extra_objects: bool,
) -> Result<PreparedTask, String> {
    if reject_extra_objects {
        validate_payload_control_names(&task)?;
    }

    let mut command = crate::bash_background::persistence::open_control_file(
        &task,
        crate::bash_background::persistence::COMMAND_FILE,
    )
    .map_err(|error| format!("failed to open command payload: {error}"))?;
    let mut wrapper = crate::bash_background::persistence::open_control_file(
        &task,
        crate::bash_background::persistence::WRAPPER_FILE,
    )
    .map_err(|error| format!("failed to open wrapper payload: {error}"))?;
    let mut environment_file = crate::bash_background::persistence::open_control_file(
        &task,
        crate::bash_background::persistence::ENVIRONMENT_FILE,
    )
    .map_err(|error| format!("failed to open environment payload: {error}"))?;
    let mut manifest = crate::bash_background::persistence::open_control_file(
        &task,
        crate::bash_background::persistence::MANIFEST_FILE,
    )
    .map_err(|error| format!("failed to open payload manifest: {error}"))?;

    let command_bytes = read_held_payload(&mut command)?;
    let wrapper_bytes = read_held_payload(&mut wrapper)?;
    let environment_bytes = read_held_payload(&mut environment_file)?;
    let manifest_bytes = read_held_payload(&mut manifest)?;
    let environment = decode_environment(&environment_bytes)?;
    let digest = payload_digest(
        &task.paths.task_id,
        &command_bytes,
        &wrapper_bytes,
        &environment_bytes,
        root,
        cwd,
        principal,
        shell_path,
        expected_environment,
    );
    if command_bytes != expected_command
        || environment != *expected_environment
        || manifest_bytes.as_slice() != digest.as_bytes()
        || expected_digest.is_some_and(|expected| expected != digest)
    {
        return Err("escalation payload manifest digest mismatch".to_string());
    }
    if reject_extra_objects {
        validate_payload_control_names(&task)?;
    }
    command
        .seek(SeekFrom::Start(0))
        .map_err(|error| format!("failed to rewind command payload: {error}"))?;
    wrapper
        .seek(SeekFrom::Start(0))
        .map_err(|error| format!("failed to rewind wrapper payload: {error}"))?;
    environment_file
        .seek(SeekFrom::Start(0))
        .map_err(|error| format!("failed to rewind environment payload: {error}"))?;
    Ok(PreparedTask(Arc::new(PreparedTaskInner {
        paths: task.paths,
        dirs: task.dirs,
        digest,
        environment,
        command_bytes: Arc::new(command_bytes),
        wrapper_bytes: Arc::new(wrapper_bytes),
        _command_file: Arc::new(command),
        _wrapper_file: Arc::new(wrapper),
        _environment_file: Arc::new(environment_file),
    })))
}

#[cfg(unix)]
fn validate_payload_control_names(
    task: &crate::bash_background::persistence::ResolvedTask,
) -> Result<(), String> {
    let mut names = task
        .dirs
        .control
        .list_names()
        .map_err(|error| format!("failed to enumerate escalation payload: {error}"))?;
    names.sort();
    let mut expected = [
        OsString::from(crate::bash_background::persistence::COMMAND_FILE),
        OsString::from(crate::bash_background::persistence::ENVIRONMENT_FILE),
        OsString::from(crate::bash_background::persistence::MANIFEST_FILE),
        OsString::from(crate::bash_background::persistence::WRAPPER_FILE),
    ];
    expected.sort();
    if names.as_slice() != expected.as_slice() {
        return Err(format!(
            "escalation payload contains a missing or extra object: {names:?}"
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn read_held_payload(file: &mut File) -> Result<Vec<u8>, String> {
    file.seek(SeekFrom::Start(0))
        .map_err(|error| format!("failed to rewind held payload: {error}"))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| format!("failed to read held payload: {error}"))?;
    Ok(bytes)
}

#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
fn payload_digest(
    task_id: &str,
    command: &[u8],
    wrapper: &[u8],
    environment_bytes: &[u8],
    root: &Path,
    cwd: &Path,
    principal: &AuthenticatedPrincipal,
    shell_path: &Path,
    environment: &ChildEnvironment,
) -> blake3::Hash {
    let mut hasher = blake3::Hasher::new();
    hash_field(&mut hasher, ESCALATION_DIGEST_TAG);
    hash_field(&mut hasher, task_id.as_bytes());
    for (role, name, bytes) in [
        (
            b"command".as_slice(),
            crate::bash_background::persistence::COMMAND_FILE,
            command,
        ),
        (
            b"wrapper".as_slice(),
            crate::bash_background::persistence::WRAPPER_FILE,
            wrapper,
        ),
        (
            b"environment".as_slice(),
            crate::bash_background::persistence::ENVIRONMENT_FILE,
            environment_bytes,
        ),
    ] {
        hash_field(&mut hasher, role);
        hash_field(&mut hasher, name.as_bytes());
        hash_field(&mut hasher, bytes);
    }
    hash_field(&mut hasher, &os_bytes(root.as_os_str()));
    hash_field(&mut hasher, &os_bytes(cwd.as_os_str()));
    hash_principal(&mut hasher, principal);
    hash_field(&mut hasher, &os_bytes(shell_path.as_os_str()));
    hash_field(&mut hasher, env!("CARGO_PKG_VERSION").as_bytes());
    hash_field(&mut hasher, ESCALATION_TIER);
    hasher.update(&(environment.len() as u64).to_be_bytes());
    for (key, value) in environment {
        hash_field(&mut hasher, &os_bytes(key));
        hash_field(&mut hasher, &os_bytes(value));
    }
    hasher.finalize()
}

#[cfg(unix)]
fn encode_environment(environment: &ChildEnvironment) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(ENVIRONMENT_TAG);
    bytes.extend_from_slice(&(environment.len() as u64).to_be_bytes());
    for (key, value) in environment {
        let key = os_bytes(key);
        let value = os_bytes(value);
        bytes.extend_from_slice(&(key.len() as u64).to_be_bytes());
        bytes.extend_from_slice(&key);
        bytes.extend_from_slice(&(value.len() as u64).to_be_bytes());
        bytes.extend_from_slice(&value);
    }
    bytes
}

#[cfg(unix)]
fn decode_environment(bytes: &[u8]) -> Result<ChildEnvironment, String> {
    use std::os::unix::ffi::OsStringExt;

    let mut cursor = ENVIRONMENT_TAG.len();
    if !bytes.starts_with(ENVIRONMENT_TAG) {
        return Err("invalid environment payload tag".to_string());
    }
    let count = read_u64(bytes, &mut cursor)?;
    let mut environment = ChildEnvironment::new();
    for _ in 0..count {
        let key_len = read_u64(bytes, &mut cursor)? as usize;
        let key = take_bytes(bytes, &mut cursor, key_len)?;
        let value_len = read_u64(bytes, &mut cursor)? as usize;
        let value = take_bytes(bytes, &mut cursor, value_len)?;
        if environment
            .insert(OsString::from_vec(key), OsString::from_vec(value))
            .is_some()
        {
            return Err("duplicate key in environment payload".to_string());
        }
    }
    if cursor != bytes.len() {
        return Err("trailing bytes in environment payload".to_string());
    }
    Ok(environment)
}

#[cfg(unix)]
fn read_u64(bytes: &[u8], cursor: &mut usize) -> Result<u64, String> {
    let field = take_bytes(bytes, cursor, 8)?;
    Ok(u64::from_be_bytes(field.try_into().map_err(|_| {
        "invalid environment length field".to_string()
    })?))
}

#[cfg(unix)]
fn take_bytes(bytes: &[u8], cursor: &mut usize, len: usize) -> Result<Vec<u8>, String> {
    let end = cursor
        .checked_add(len)
        .filter(|end| *end <= bytes.len())
        .ok_or_else(|| "truncated environment payload".to_string())?;
    let value = bytes[*cursor..end].to_vec();
    *cursor = end;
    Ok(value)
}

#[cfg(unix)]
fn hash_principal(hasher: &mut blake3::Hasher, principal: &AuthenticatedPrincipal) {
    match principal {
        AuthenticatedPrincipal::FirstParty => hash_field(hasher, b"first_party"),
        AuthenticatedPrincipal::RouteBind {
            trust,
            route_channel,
            route_epoch,
            project_root,
            harness,
            session_id,
            principal_id,
        } => {
            hash_field(hasher, b"route_bind");
            hash_field(
                hasher,
                match trust {
                    PrincipalTrust::FirstParty => b"first_party",
                    PrincipalTrust::Untrusted => b"untrusted",
                },
            );
            hasher.update(&route_channel.to_be_bytes());
            hasher.update(&route_epoch.to_be_bytes());
            hash_field(hasher, &os_bytes(project_root.as_os_str()));
            hash_field(hasher, harness.as_bytes());
            hash_field(hasher, session_id.as_bytes());
            match principal_id {
                Some(id) => {
                    hasher.update(&[1]);
                    hash_field(hasher, id.as_bytes());
                }
                None => {
                    hasher.update(&[0]);
                }
            }
        }
    }
}

#[cfg(unix)]
fn hash_field(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

#[cfg(unix)]
fn os_bytes(value: &OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    value.as_bytes().to_vec()
}

#[cfg(unix)]
fn hex_bytes(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

/// Returns true on Unix when native sandboxing is enabled for a first-party caller.
///
/// Process spawning and in-process bash rewriting both use this check so the
/// command executes through the configured sandbox instead of being rewritten
/// to run inside the unsandboxed AFT process.
pub(crate) fn native_sandbox_enforced(
    ctx: &AppContext,
    principal: &AuthenticatedPrincipal,
) -> bool {
    cfg!(unix) && ctx.config().sandbox.enabled && principal_is_first_party(principal)
}

pub(crate) fn unsupported_platform_sandbox_refusal(ctx: &AppContext) -> Option<SpawnPlan> {
    (ctx.config().sandbox.enabled && !cfg!(unix)).then(|| SpawnPlan::Refused {
        code: "sandbox_unavailable",
        message: "sandbox is not supported on this platform; disable sandbox.enabled or run on macOS/Linux"
            .to_string(),
        mismatch_class: None,
    })
}

/// Resolve policy for an agent-command process.
///
/// `task_bundle_dir` must be the already-created directory that owns the task's
/// capture files. The native builder creates a fresh private temp directory
/// beneath it and includes both directories in the profile.
pub fn resolve_sandbox_spawn(
    ctx: &AppContext,
    principal: &AuthenticatedPrincipal,
    requested_tier: RequestedSandboxTier,
    task_kind: SandboxTaskKind,
    task_bundle_dir: &Path,
    host_escalation: Option<&HostEscalationAttempt>,
) -> SpawnPlan {
    note_test_observation(ctx, principal, requested_tier, task_kind);

    // This check must remain ahead of every platform and production-policy
    // branch. Windows tests use it to avoid entering process paths that cannot
    // consume native launcher plans.
    #[cfg(test)]
    if let Some(plan) = TEST_PLAN_OVERRIDE.with(|slot| slot.borrow().clone()) {
        return plan;
    }

    // An enabled policy must never degrade into an ordinary child merely
    // because this build has no kernel sandbox backend.
    if let Some(refusal) = unsupported_platform_sandbox_refusal(ctx) {
        return refusal;
    }

    if requested_tier == RequestedSandboxTier::Disabled || !ctx.config().sandbox.enabled {
        return SpawnPlan::Unsandboxed;
    }

    if requested_tier == RequestedSandboxTier::Host {
        if !principal_is_first_party(principal) {
            return SpawnPlan::Refused {
                code: "sandbox_escalation_denied",
                message: "sandbox host escalation is unavailable to untrusted principals"
                    .to_string(),
                mismatch_class: Some("wrong_principal"),
            };
        }

        #[cfg(windows)]
        {
            let _ = (ctx, task_kind, task_bundle_dir, host_escalation);
            unreachable!("unsupported platforms return before host-tier resolution");
        }

        #[cfg(unix)]
        {
            let Some(attempt) = host_escalation else {
                return escalation_refused(EscalationRefusal::DigestMismatch);
            };
            return match consume_host_escalation_grant_at(ctx, principal, attempt, Instant::now()) {
                Ok(prepared) => SpawnPlan::Host {
                    shell_path: attempt.shell_path.clone(),
                    environment: prepared.environment().clone(),
                }
                .with_prepared_task(prepared),
                Err(refusal) => escalation_refused(refusal),
            };
        }

        #[cfg(all(not(unix), not(windows)))]
        {
            let _ = (ctx, task_kind, task_bundle_dir, host_escalation);
            unreachable!("unsupported platforms return before host-tier resolution");
        }
    }

    if !native_sandbox_enforced(ctx, principal) {
        return SpawnPlan::Unsandboxed;
    }

    #[cfg(windows)]
    {
        let _ = (ctx, task_kind, task_bundle_dir);
        unreachable!("unsupported platforms return before native-tier resolution")
    }

    #[cfg(unix)]
    {
        let profile = match build_native_profile(ctx, principal, task_bundle_dir) {
            Ok(profile) => profile,
            Err(error) => {
                return sandbox_setup_refusal(ctx, principal, error);
            }
        };
        let launcher_path = match std::env::current_exe() {
            Ok(path) => path,
            Err(error) => {
                let _ = std::fs::remove_dir_all(&profile.temp_dir);
                return sandbox_setup_refusal(
                    ctx,
                    principal,
                    format!("failed to locate the aft executable: {error}"),
                );
            }
        };
        crate::slog_info!(
            "sandbox profile apply: tier=native task_kind={task_kind:?} writable_roots={} read_deny={}",
            profile.writable_roots.len(),
            profile.read_deny.len()
        );
        crate::slog_debug!(
            "sandbox profile paths: tier=native writable_roots={:?} write_deny_nested={:?} read_deny={:?} socket_deny={:?} cache_roots={:?} temp_dir={:?}",
            profile.writable_roots,
            profile.write_deny_nested,
            profile.read_deny,
            profile.socket_deny,
            profile.cache_roots,
            profile.temp_dir
        );
        SpawnPlan::Launcher {
            profile,
            launcher_path,
        }
    }

    #[cfg(all(not(unix), not(windows)))]
    {
        let _ = (ctx, principal, task_kind, task_bundle_dir);
        unreachable!("unsupported platforms return before native-tier resolution")
    }
}

#[cfg(unix)]
fn sandbox_setup_refusal(
    ctx: &AppContext,
    principal: &AuthenticatedPrincipal,
    cause: impl std::fmt::Display,
) -> SpawnPlan {
    let config = ctx.config();
    let root = config
        .project_root
        .as_deref()
        .map_or_else(|| Path::new("<unknown>"), |root| root);
    let session = match principal {
        AuthenticatedPrincipal::RouteBind { session_id, .. } => session_id.clone(),
        AuthenticatedPrincipal::FirstParty => {
            crate::log_ctx::current_session().unwrap_or_else(|| "<unknown>".to_string())
        }
    };
    let message = format!(
        "sandbox setup for {} failed: {cause}; set sandbox.enabled=false to disable native sandboxing",
        root.display()
    );
    crate::slog_warn!("{message}; root={} session={session}", root.display());
    SpawnPlan::Refused {
        code: "sandbox_unavailable",
        message,
        mismatch_class: None,
    }
}

#[cfg(unix)]
fn escalation_refused(refusal: EscalationRefusal) -> SpawnPlan {
    let class = refusal.class();
    SpawnPlan::Refused {
        code: "sandbox_escalation_denied",
        message: format!("sandbox host escalation grant refused: {class}"),
        mismatch_class: Some(class),
    }
}

#[cfg(unix)]
fn build_native_profile(
    ctx: &AppContext,
    principal: &AuthenticatedPrincipal,
    task_bundle_dir: &Path,
) -> Result<SandboxProfile, String> {
    let home = std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| {
            "HOME is not set, so credential and cache paths cannot be resolved".to_string()
        })?;
    if !home.is_absolute() {
        return Err(format!("HOME must be absolute: {}", home.display()));
    }
    let home = home
        .canonicalize()
        .map_err(|error| format!("failed to canonicalize HOME {}: {error}", home.display()))?;
    if !home.is_dir() {
        return Err(format!("HOME is not a directory: {}", home.display()));
    }

    let mut project_roots = Vec::new();
    if let Some(root) = &ctx.config().project_root {
        project_roots.push(root.clone());
    }
    if let AuthenticatedPrincipal::RouteBind { project_root, .. } = principal {
        if !project_roots.contains(project_root) {
            project_roots.push(project_root.clone());
        }
    }
    if project_roots.is_empty() {
        project_roots.push(
            std::env::current_dir()
                .map_err(|error| format!("failed to resolve the current project root: {error}"))?,
        );
    }

    for root in &mut project_roots {
        if !root.is_dir() {
            return Err(format!(
                "project root is not an existing directory: {}",
                root.display()
            ));
        }
        *root = root.canonicalize().map_err(|error| {
            format!(
                "failed to canonicalize project root {}: {error}",
                root.display()
            )
        })?;
    }
    project_roots.sort_unstable();
    project_roots.dedup();

    if !task_bundle_dir.is_dir() {
        return Err(format!(
            "task io directory is not an existing directory: {}",
            task_bundle_dir.display()
        ));
    }
    let task_io_dir = task_bundle_dir
        .canonicalize()
        .map_err(|error| format!("failed to canonicalize task io directory: {error}"))?;
    let session_store = session_store_for_task_io(&task_io_dir)?;

    let git_policies = project_roots
        .iter()
        .map(|root| resolve_git_policy(root))
        .collect::<Result<Vec<_>, _>>()?;
    let temp_dir = create_task_temp_dir(&task_io_dir)?;
    let result = (|| {
        let mut writable_roots = project_roots.clone();
        writable_roots.push(task_io_dir.clone());
        writable_roots.extend(
            ctx.config()
                .sandbox
                .write_allow
                .iter()
                .map(|path| expand_home(path, &home)),
        );

        let secret_floor = vec![
            home.join(".ssh"),
            home.join(".aws"),
            home.join(".gnupg"),
            home.join(".config/gcloud"),
            home.join(".azure"),
            home.join(".config/cortexkit"),
        ];
        // The credential floor denies both read and write. Linux rejects any
        // writable overlap because Landlock cannot subtract write rights.
        let write_deny = secret_floor.clone();
        #[cfg(target_os = "macos")]
        let mut write_deny = write_deny;
        let mut write_deny_nested = Vec::new();
        let mut read_deny = secret_floor;
        for (root, git_policy) in project_roots.iter().zip(&git_policies) {
            #[cfg(target_os = "linux")]
            write_deny_nested.push(root.join(".git"));
            write_deny_nested.push(root.join(".cortexkit"));
            #[cfg(target_os = "macos")]
            write_deny.extend(git_policy.hooks.iter().cloned());
            read_deny.extend(git_policy.hooks.iter().cloned());
        }
        #[cfg(target_os = "linux")]
        read_deny.extend([
            PathBuf::from("/run/user"),
            PathBuf::from("/run/credentials"),
            PathBuf::from("/run/secrets"),
        ]);
        read_deny.extend(
            ctx.config()
                .sandbox
                .read_deny
                .iter()
                .map(|path| expand_home(path, &home)),
        );

        let mut cache_roots = vec![
            home.join(".cargo/registry"),
            home.join(".cargo/git"),
            home.join(".rustup/downloads"),
            home.join(".npm"),
            home.join(".bun/install/cache"),
            home.join(".cache/pip"),
            home.join(".cache/uv"),
            home.join(".cache/go-build"),
            home.join(".gradle/caches"),
            home.join(".m2/repository"),
        ];
        #[cfg(target_os = "macos")]
        cache_roots.extend([
            home.join("Library/Caches/pip"),
            home.join("Library/Caches/uv"),
            home.join("Library/Caches/go-build"),
        ]);
        cache_roots.retain(|path| path.is_dir());

        let mut socket_deny = vec![PathBuf::from("/var/run/docker.sock")];
        if let Some(agent_socket) =
            std::env::var_os("SSH_AUTH_SOCK").filter(|value| !value.is_empty())
        {
            socket_deny.push(PathBuf::from(agent_socket));
        }

        let mut profile = SandboxProfile::build(
            writable_roots,
            write_deny,
            write_deny_nested,
            Vec::new(),
            read_deny,
            socket_deny,
            cache_roots,
            temp_dir.clone(),
        )
        .map_err(|error| error.to_string())?;
        // Seatbelt starts from allow-all reads, so it must deny the complete
        // store. Landlock instead omits the store while splitting read grants,
        // then adds only the prepared task's exact payload files.
        #[cfg(target_os = "macos")]
        if !profile.read_deny.contains(&session_store) {
            profile.read_deny.push(session_store.clone());
        }
        refuse_store_overlap(&profile, &session_store, &task_io_dir)?;

        #[cfg(target_os = "linux")]
        let profile = {
            let git_read_roots = git_policies
                .iter()
                .flat_map(|policy| policy.read_roots.iter().cloned())
                .collect::<Vec<_>>();
            profile.read_allow = build_linux_read_allow(
                &profile,
                &home,
                &git_read_roots,
                std::slice::from_ref(&session_store),
            )?;
            profile = profile
                .canonicalize_for_launch()
                .map_err(|error| error.to_string())?;
            validate_final_read_rules(&profile.read_allow, &profile.read_deny)?;
            assert!(
                validate_final_read_rules(&profile.read_allow, &profile.read_deny).is_ok(),
                "final Landlock read grants overlap a denied path"
            );
            profile
        };

        Ok(profile)
    })();
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&temp_dir);
    }
    result
}

#[cfg(unix)]
pub(crate) fn control_payload_read_grants(task_io: &Path) -> Result<Vec<PathBuf>, String> {
    if task_io.file_name() != Some(OsStr::new("io")) {
        return Err("task payload grants require the directory-layout io path".to_string());
    }
    let task_dir = task_io
        .parent()
        .ok_or_else(|| "task io directory has no task parent".to_string())?;
    let task_id = task_dir
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| "task directory has no UTF-8 identity".to_string())?;
    crate::bash_background::persistence::validate_task_id(task_id)
        .map_err(|error| error.to_string())?;
    let control = task_dir.join("control");
    Ok(vec![
        control.join(crate::bash_background::persistence::COMMAND_FILE),
        control.join(crate::bash_background::persistence::WRAPPER_FILE),
        control.join(crate::bash_background::persistence::ENVIRONMENT_FILE),
    ])
}

#[cfg(unix)]
fn session_store_for_task_io(task_io: &Path) -> Result<PathBuf, String> {
    let Some(task_dir) = task_io.parent() else {
        return Err("task io directory has no task parent".to_string());
    };
    let directory_layout = task_io.file_name() == Some(OsStr::new("io"))
        && task_dir
            .file_name()
            .and_then(OsStr::to_str)
            .is_some_and(|task_id| {
                crate::bash_background::persistence::validate_task_id(task_id).is_ok()
            });
    let candidate = if directory_layout {
        task_dir
            .parent()
            .ok_or_else(|| "task directory has no session parent".to_string())?
    } else {
        task_io
    };
    candidate
        .canonicalize()
        .map_err(|error| format!("failed to canonicalize bash task session store: {error}"))
}

#[cfg(unix)]
fn refuse_store_overlap(
    profile: &SandboxProfile,
    session_store: &Path,
    task_io: &Path,
) -> Result<(), String> {
    for root in profile.write_allow_roots() {
        if root == task_io || root.starts_with(task_io) {
            continue;
        }
        if root == session_store
            || root.starts_with(session_store)
            || session_store.starts_with(root)
        {
            return Err(format!(
                "sandbox writable root overlaps the bash task session store: writable={} store={}",
                root.display(),
                session_store.display()
            ));
        }
    }
    Ok(())
}

#[cfg(unix)]
#[derive(Debug)]
struct GitPolicy {
    #[cfg(target_os = "linux")]
    read_roots: Vec<PathBuf>,
    hooks: Vec<PathBuf>,
}

#[cfg(unix)]
fn resolve_git_policy(project_root: &Path) -> Result<GitPolicy, String> {
    let dot_git = project_root.join(".git");
    let metadata = match std::fs::symlink_metadata(&dot_git) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(GitPolicy {
                #[cfg(target_os = "linux")]
                read_roots: Vec::new(),
                hooks: vec![dot_git.join("hooks")],
            });
        }
        Err(error) => {
            return Err(format!(
                "failed to inspect Git metadata {}: {error}",
                dot_git.display()
            ));
        }
    };
    if metadata.file_type().is_symlink() {
        return Err(format!(
            "refusing sandbox profile with symlinked Git metadata: {}",
            dot_git.display()
        ));
    }

    let git_dir = if metadata.is_dir() {
        dot_git.canonicalize().map_err(|error| {
            format!(
                "failed to canonicalize Git directory {}: {error}",
                dot_git.display()
            )
        })?
    } else if metadata.is_file() {
        let pointer = std::fs::read_to_string(&dot_git).map_err(|error| {
            format!(
                "failed to read linked-worktree Git pointer {}: {error}",
                dot_git.display()
            )
        })?;
        let pointer = pointer
            .trim()
            .strip_prefix("gitdir:")
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .ok_or_else(|| {
                format!(
                    "linked-worktree Git pointer is malformed: {}",
                    dot_git.display()
                )
            })?;
        let pointer = PathBuf::from(pointer);
        let pointer = if pointer.is_absolute() {
            pointer
        } else {
            project_root.join(pointer)
        };
        pointer.canonicalize().map_err(|error| {
            format!(
                "failed to resolve linked-worktree Git directory {}: {error}",
                pointer.display()
            )
        })?
    } else {
        return Err(format!(
            "Git metadata is neither a file nor directory: {}",
            dot_git.display()
        ));
    };
    if !git_dir.is_dir() {
        return Err(format!(
            "resolved Git directory is not a directory: {}",
            git_dir.display()
        ));
    }

    let commondir_file = git_dir.join("commondir");
    let common_dir = match std::fs::read_to_string(&commondir_file) {
        Ok(value) => {
            let value = value.trim();
            if value.is_empty() {
                return Err(format!(
                    "Git commondir pointer is empty: {}",
                    commondir_file.display()
                ));
            }
            let value = PathBuf::from(value);
            let value = if value.is_absolute() {
                value
            } else {
                git_dir.join(value)
            };
            value.canonicalize().map_err(|error| {
                format!(
                    "failed to resolve Git commondir {}: {error}",
                    value.display()
                )
            })?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => git_dir.clone(),
        Err(error) => {
            return Err(format!(
                "failed to read Git commondir {}: {error}",
                commondir_file.display()
            ));
        }
    };
    if !common_dir.is_dir() {
        return Err(format!(
            "resolved Git commondir is not a directory: {}",
            common_dir.display()
        ));
    }

    let hooks = resolve_hooks_path(project_root, &common_dir)?;
    #[cfg(target_os = "linux")]
    let read_roots = {
        let mut read_roots = vec![git_dir, common_dir];
        read_roots.sort_unstable();
        read_roots.dedup();
        read_roots
    };
    Ok(GitPolicy {
        #[cfg(target_os = "linux")]
        read_roots,
        hooks: vec![hooks],
    })
}

#[cfg(unix)]
fn resolve_hooks_path(project_root: &Path, common_dir: &Path) -> Result<PathBuf, String> {
    let configured = Command::new("git")
        .arg("-C")
        .arg(project_root)
        .args(["config", "--path", "core.hooksPath"])
        .output()
        .map_err(|error| {
            format!(
                "failed to query core.hooksPath for {}: {error}",
                project_root.display()
            )
        })?;
    if configured.status.success() {
        let configured = String::from_utf8(configured.stdout).map_err(|error| {
            format!(
                "core.hooksPath for {} is not UTF-8: {error}",
                project_root.display()
            )
        })?;
        if configured.trim().is_empty() {
            return Err(format!(
                "core.hooksPath for {} is empty",
                project_root.display()
            ));
        }
        let resolved = Command::new("git")
            .arg("-C")
            .arg(project_root)
            .args(["rev-parse", "--path-format=absolute", "--git-path", "hooks"])
            .output()
            .map_err(|error| {
                format!(
                    "failed to resolve core.hooksPath for {}: {error}",
                    project_root.display()
                )
            })?;
        if !resolved.status.success() {
            return Err(format!(
                "git could not resolve core.hooksPath for {}: {}",
                project_root.display(),
                String::from_utf8_lossy(&resolved.stderr).trim()
            ));
        }
        let resolved = String::from_utf8(resolved.stdout).map_err(|error| {
            format!(
                "resolved core.hooksPath for {} is not UTF-8: {error}",
                project_root.display()
            )
        })?;
        let resolved = PathBuf::from(resolved.trim());
        if !resolved.is_absolute() {
            return Err(format!(
                "git returned a non-absolute core.hooksPath for {}: {}",
                project_root.display(),
                resolved.display()
            ));
        }
        return canonicalize_policy_path(resolved, "core.hooksPath");
    }

    if configured.status.code() != Some(1) || !configured.stdout.is_empty() {
        return Err(format!(
            "git could not query core.hooksPath for {}: {}",
            project_root.display(),
            String::from_utf8_lossy(&configured.stderr).trim()
        ));
    }
    canonicalize_policy_path(common_dir.join("hooks"), "Git hooks")
}

#[cfg(unix)]
fn canonicalize_policy_path(path: PathBuf, field: &str) -> Result<PathBuf, String> {
    match path.canonicalize() {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut ancestor = path.clone();
            let mut tail = Vec::new();
            loop {
                match ancestor.canonicalize() {
                    Ok(mut canonical) => {
                        for component in tail.iter().rev() {
                            canonical.push(component);
                        }
                        return Ok(canonical);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        let component =
                            ancestor.file_name().map(ToOwned::to_owned).ok_or_else(|| {
                                format!(
                                    "failed to canonicalize {field} path {}: {error}",
                                    path.display()
                                )
                            })?;
                        tail.push(component);
                        if !ancestor.pop() {
                            return Err(format!(
                                "failed to canonicalize {field} path {}: {error}",
                                path.display()
                            ));
                        }
                    }
                    Err(error) => {
                        return Err(format!(
                            "failed to canonicalize {field} path {}: {error}",
                            path.display()
                        ));
                    }
                }
            }
        }
        Err(error) => Err(format!(
            "failed to canonicalize {field} path {}: {error}",
            path.display()
        )),
    }
}

#[cfg(any(test, target_os = "linux"))]
#[derive(Debug, Clone)]
struct IntendedReadGrant {
    path: PathBuf,
    force_children: bool,
    mandatory: bool,
}

#[cfg(any(test, target_os = "linux"))]
#[derive(Debug, Clone, PartialEq, Eq)]
struct ListedReadChild {
    path: PathBuf,
    is_dir: bool,
}

#[cfg(any(test, target_os = "linux"))]
trait ReadDirectoryLister {
    fn children(&mut self, parent: &Path) -> Result<Vec<ListedReadChild>, String>;
}

#[cfg(target_os = "linux")]
fn build_linux_read_allow(
    profile: &SandboxProfile,
    home: &Path,
    git_read_roots: &[PathBuf],
    omitted_roots: &[PathBuf],
) -> Result<Vec<PathBuf>, String> {
    let mandatory_floor = &profile.write_deny;
    validate_mandatory_floor_overlap(profile.write_allow_roots(), mandatory_floor)?;

    let mut intended = Vec::new();
    for path in [
        "/usr",
        "/bin",
        "/sbin",
        "/lib",
        "/lib32",
        "/lib64",
        "/etc",
        "/opt",
        "/run",
        "/proc",
        "/sys/devices/system/cpu",
        "/sys/fs/cgroup",
        "/dev/null",
        "/dev/zero",
        "/dev/full",
        "/dev/random",
        "/dev/urandom",
        "/dev/tty",
        "/dev/ptmx",
        "/dev/pts",
        "/dev/fd",
        "/dev/stdin",
        "/dev/stdout",
        "/dev/stderr",
    ] {
        if let Some(path) = canonicalize_existing_static(Path::new(path))? {
            intended.push(IntendedReadGrant {
                path,
                force_children: false,
                mandatory: true,
            });
        }
    }
    if let Some(path) = canonicalize_existing_static(Path::new("/var"))? {
        intended.push(IntendedReadGrant {
            path,
            // Enumerating /var avoids following the /var/run symlink back into /run.
            force_children: true,
            mandatory: true,
        });
    }

    intended.push(IntendedReadGrant {
        path: home.to_path_buf(),
        force_children: true,
        mandatory: false,
    });
    intended.extend(
        profile
            .write_allow_roots()
            .into_iter()
            .map(|path| IntendedReadGrant {
                path: path.to_path_buf(),
                force_children: false,
                mandatory: false,
            }),
    );
    intended.extend(
        git_read_roots
            .iter()
            .cloned()
            .map(|path| IntendedReadGrant {
                path,
                force_children: false,
                mandatory: false,
            }),
    );

    let split_denies = profile
        .read_deny
        .iter()
        .chain(omitted_roots)
        .cloned()
        .collect::<Vec<_>>();
    let mut lister = SecureReadDirectoryLister;
    split_read_grants(&intended, &split_denies, &mut lister)
}

#[cfg(target_os = "linux")]
fn add_linux_payload_read_grants(
    profile: &mut SandboxProfile,
    payload_grants: &[PathBuf],
) -> Result<(), String> {
    let intended = payload_grants
        .iter()
        .map(|path| {
            let path = path.canonicalize().map_err(|error| {
                format!(
                    "mandatory payload read path is unavailable: {}: {error}",
                    path.display()
                )
            })?;
            Ok(IntendedReadGrant {
                path,
                force_children: false,
                mandatory: true,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let mut lister = SecureReadDirectoryLister;
    let payload_grants = split_read_grants(&intended, &profile.read_deny, &mut lister)?;

    let mut final_read_allow = profile.read_allow.clone();
    final_read_allow.extend(payload_grants);
    final_read_allow.sort_unstable();
    final_read_allow.dedup();
    validate_final_read_rules(&final_read_allow, &profile.read_deny)?;
    assert!(
        validate_final_read_rules(&final_read_allow, &profile.read_deny).is_ok(),
        "final Landlock read grants overlap a denied path after adding payload files"
    );
    profile.read_allow = final_read_allow;
    Ok(())
}

#[cfg(target_os = "linux")]
fn canonicalize_existing_static(path: &Path) -> Result<Option<PathBuf>, String> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => match path.canonicalize() {
            Ok(path) => Ok(Some(path)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(format!(
                "failed to canonicalize static read root {}: {error}",
                path.display()
            )),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!(
            "failed to inspect static read root {}: {error}",
            path.display()
        )),
    }
}

#[cfg(any(test, target_os = "linux"))]
fn split_read_grants(
    intended: &[IntendedReadGrant],
    denies: &[PathBuf],
    lister: &mut impl ReadDirectoryLister,
) -> Result<Vec<PathBuf>, String> {
    let mut emitted = BTreeSet::new();
    for grant in intended {
        split_read_grant(grant, denies, lister, &mut emitted)?;
    }
    let emitted = emitted.into_iter().collect::<Vec<_>>();
    validate_final_read_rules(&emitted, denies)?;
    Ok(emitted)
}

#[cfg(any(test, target_os = "linux"))]
fn split_read_grant(
    grant: &IntendedReadGrant,
    denies: &[PathBuf],
    lister: &mut impl ReadDirectoryLister,
    emitted: &mut BTreeSet<PathBuf>,
) -> Result<(), String> {
    if let Some(deny) = denies
        .iter()
        .find(|deny| grant.path == **deny || grant.path.starts_with(deny))
    {
        if grant.mandatory {
            return Err(format!(
                "sandbox_unavailable: mandatory read root {} is denied by {}",
                grant.path.display(),
                deny.display()
            ));
        }
        return Ok(());
    }

    let contains_deny = denies.iter().any(|deny| deny.starts_with(&grant.path));
    if !grant.force_children && !contains_deny {
        emitted.insert(grant.path.clone());
        return Ok(());
    }

    let children = lister.children(&grant.path).map_err(|error| {
        format!(
            "sandbox_unavailable: cannot split read root {}: {error}",
            grant.path.display()
        )
    })?;
    for child in children {
        let child_contains_deny = denies.iter().any(|deny| deny.starts_with(&child.path));
        if child_contains_deny && !child.is_dir {
            return Err(format!(
                "sandbox_unavailable: deny chain crosses non-directory path {}",
                child.path.display()
            ));
        }
        split_read_grant(
            &IntendedReadGrant {
                path: child.path,
                force_children: false,
                mandatory: false,
            },
            denies,
            lister,
            emitted,
        )?;
    }
    Ok(())
}

#[cfg(any(test, target_os = "linux"))]
fn validate_mandatory_floor_overlap<'a>(
    writable_roots: impl IntoIterator<Item = &'a Path>,
    mandatory_floor: &[PathBuf],
) -> Result<(), String> {
    for writable in writable_roots {
        for secret in mandatory_floor {
            if paths_overlap(writable, secret) {
                return Err(format!(
                    "writable root {} overlaps mandatory secret floor {}",
                    writable.display(),
                    secret.display()
                ));
            }
        }
    }
    Ok(())
}

#[cfg(any(test, target_os = "linux"))]
fn validate_final_read_rules(read_allow: &[PathBuf], denies: &[PathBuf]) -> Result<(), String> {
    for grant in read_allow {
        for deny in denies {
            if paths_overlap(grant, deny) {
                return Err(format!(
                    "sandbox_unavailable: final read grant {} overlaps denied path {}",
                    grant.display(),
                    deny.display()
                ));
            }
        }
    }
    Ok(())
}

#[cfg(any(test, target_os = "linux"))]
fn paths_overlap(left: &Path, right: &Path) -> bool {
    left == right || left.starts_with(right) || right.starts_with(left)
}

#[cfg(target_os = "linux")]
struct SecureReadDirectoryLister;

#[cfg(target_os = "linux")]
impl ReadDirectoryLister for SecureReadDirectoryLister {
    fn children(&mut self, parent: &Path) -> Result<Vec<ListedReadChild>, String> {
        let parent_fd = open_absolute_no_symlinks(parent, true)?;
        let readable_fd =
            open_directory_for_enumeration(parent_fd.as_raw_fd()).map_err(|error| {
                format!(
                    "failed to open directory for enumeration {}: {error}",
                    parent.display()
                )
            })?;
        let duplicate = unsafe { libc::dup(readable_fd.as_raw_fd()) };
        if duplicate < 0 {
            return Err(format!(
                "failed to duplicate directory fd for {}: {}",
                parent.display(),
                std::io::Error::last_os_error()
            ));
        }
        let directory = unsafe { libc::fdopendir(duplicate) };
        if directory.is_null() {
            let error = std::io::Error::last_os_error();
            unsafe { libc::close(duplicate) };
            return Err(format!(
                "failed to enumerate directory {}: {error}",
                parent.display()
            ));
        }

        let result = (|| {
            let mut children = Vec::new();
            loop {
                unsafe { *libc::__errno_location() = 0 };
                let entry = unsafe { libc::readdir(directory) };
                if entry.is_null() {
                    let error = std::io::Error::last_os_error();
                    if error.raw_os_error() == Some(0) {
                        break;
                    }
                    return Err(format!(
                        "failed while enumerating {}: {error}",
                        parent.display()
                    ));
                }
                let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
                if name == b"." || name == b".." {
                    continue;
                }
                let name = OsStr::from_bytes(name);
                let diagnostic_path = parent.join(name);
                let diagnostic = std::fs::symlink_metadata(&diagnostic_path).map_err(|error| {
                    format!(
                        "directory entry changed while inspecting {}: {error}",
                        diagnostic_path.display()
                    )
                })?;
                if diagnostic.file_type().is_symlink() {
                    continue;
                }

                let child_fd =
                    open_child_no_symlinks(parent_fd.as_raw_fd(), name).map_err(|error| {
                        format!(
                            "directory entry changed while opening {}: {error}",
                            diagnostic_path.display()
                        )
                    })?;
                let metadata = fstat_fd(&child_fd).map_err(|error| {
                    format!(
                        "failed to inspect opened directory entry {}: {error}",
                        diagnostic_path.display()
                    )
                })?;
                if metadata.st_mode & libc::S_IFMT == libc::S_IFLNK {
                    continue;
                }
                children.push(ListedReadChild {
                    path: diagnostic_path,
                    is_dir: metadata.st_mode & libc::S_IFMT == libc::S_IFDIR,
                });
            }
            children.sort_unstable_by(|left, right| left.path.cmp(&right.path));
            Ok(children)
        })();
        unsafe { libc::closedir(directory) };
        result
    }
}

#[cfg(target_os = "linux")]
#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

#[cfg(target_os = "linux")]
const RESOLVE_NO_SYMLINKS: u64 = 0x04;
#[cfg(target_os = "linux")]
const RESOLVE_BENEATH: u64 = 0x08;

#[cfg(target_os = "linux")]
fn open_absolute_no_symlinks(path: &Path, directory: bool) -> Result<OwnedFd, String> {
    if !path.is_absolute() {
        return Err(format!("path is not absolute: {}", path.display()));
    }
    let root = unsafe {
        libc::open(
            c"/".as_ptr(),
            libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if root < 0 {
        return Err(format!(
            "failed to open filesystem root: {}",
            std::io::Error::last_os_error()
        ));
    }
    let root = unsafe { OwnedFd::from_raw_fd(root) };
    let components = normalized_relative_components(path)?;
    if components.is_empty() {
        return Ok(root);
    }

    let relative = components
        .iter()
        .fold(PathBuf::new(), |path, component| path.join(component));
    let relative = CString::new(relative.as_os_str().as_bytes())
        .map_err(|_| format!("path contains NUL: {}", path.display()))?;
    let flags = libc::O_PATH | libc::O_CLOEXEC | if directory { libc::O_DIRECTORY } else { 0 };
    let how = OpenHow {
        flags: flags as u64,
        mode: 0,
        resolve: RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS,
    };
    let opened = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            root.as_raw_fd(),
            relative.as_ptr(),
            &how,
            std::mem::size_of::<OpenHow>(),
        ) as libc::c_int
    };
    if opened >= 0 {
        return Ok(unsafe { OwnedFd::from_raw_fd(opened) });
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() != Some(libc::ENOSYS) {
        return Err(format!(
            "secure open failed for {}: {error}",
            path.display()
        ));
    }

    let mut current = root;
    for (index, component) in components.iter().enumerate() {
        let component = CString::new(component.as_bytes())
            .map_err(|_| format!("path contains NUL: {}", path.display()))?;
        let last = index + 1 == components.len();
        let mut flags = libc::O_PATH | libc::O_CLOEXEC | libc::O_NOFOLLOW;
        if !last || directory {
            flags |= libc::O_DIRECTORY;
        }
        let opened = unsafe { libc::openat(current.as_raw_fd(), component.as_ptr(), flags) };
        if opened < 0 {
            return Err(format!(
                "component-wise secure open failed for {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            ));
        }
        let opened = unsafe { OwnedFd::from_raw_fd(opened) };
        let metadata = fstat_fd(&opened)
            .map_err(|error| format!("failed to inspect {}: {error}", path.display()))?;
        if metadata.st_mode & libc::S_IFMT == libc::S_IFLNK {
            return Err(format!(
                "secure open encountered a symlink: {}",
                path.display()
            ));
        }
        current = opened;
    }
    Ok(current)
}

#[cfg(target_os = "linux")]
fn open_directory_for_enumeration(parent_fd: i32) -> Result<OwnedFd, std::io::Error> {
    let how = OpenHow {
        flags: (libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC) as u64,
        mode: 0,
        resolve: RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS,
    };
    let opened = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            parent_fd,
            c".".as_ptr(),
            &how,
            std::mem::size_of::<OpenHow>(),
        ) as libc::c_int
    };
    if opened >= 0 {
        return Ok(unsafe { OwnedFd::from_raw_fd(opened) });
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() != Some(libc::ENOSYS) {
        return Err(error);
    }

    let opened = unsafe {
        libc::openat(
            parent_fd,
            c".".as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if opened < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(unsafe { OwnedFd::from_raw_fd(opened) })
    }
}

#[cfg(target_os = "linux")]
fn open_child_no_symlinks(parent_fd: i32, name: &OsStr) -> Result<OwnedFd, std::io::Error> {
    let name = CString::new(name.as_bytes())
        .map_err(|_| std::io::Error::from_raw_os_error(libc::EINVAL))?;
    let how = OpenHow {
        flags: (libc::O_PATH | libc::O_CLOEXEC) as u64,
        mode: 0,
        resolve: RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS,
    };
    let opened = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            parent_fd,
            name.as_ptr(),
            &how,
            std::mem::size_of::<OpenHow>(),
        ) as libc::c_int
    };
    if opened >= 0 {
        return Ok(unsafe { OwnedFd::from_raw_fd(opened) });
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() != Some(libc::ENOSYS) {
        return Err(error);
    }

    let opened = unsafe {
        libc::openat(
            parent_fd,
            name.as_ptr(),
            libc::O_PATH | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if opened < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let opened = unsafe { OwnedFd::from_raw_fd(opened) };
    let metadata = fstat_fd(&opened)?;
    if metadata.st_mode & libc::S_IFMT == libc::S_IFLNK {
        return Err(std::io::Error::from_raw_os_error(libc::ELOOP));
    }
    Ok(opened)
}

#[cfg(target_os = "linux")]
fn fstat_fd(fd: &OwnedFd) -> Result<libc::stat, std::io::Error> {
    let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(fd.as_raw_fd(), metadata.as_mut_ptr()) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { metadata.assume_init() })
}

#[cfg(target_os = "linux")]
fn normalized_relative_components(path: &Path) -> Result<Vec<&OsStr>, String> {
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::RootDir => {}
            std::path::Component::Normal(component) => components.push(component),
            _ => {
                return Err(format!(
                    "path is not normalized for secure open: {}",
                    path.display()
                ));
            }
        }
    }
    Ok(components)
}

#[cfg(unix)]
fn expand_home(path: &Path, home: &Path) -> PathBuf {
    let mut components = path.components();
    if components
        .next()
        .is_some_and(|component| component.as_os_str() == "~")
    {
        return components.fold(home.to_path_buf(), |resolved, component| {
            resolved.join(component.as_os_str())
        });
    }
    path.to_path_buf()
}

#[cfg(unix)]
fn create_task_temp_dir(task_bundle_dir: &Path) -> Result<PathBuf, String> {
    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
    for _ in 0..32 {
        let nonce = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path = task_bundle_dir.join(format!(".sandbox-tmp-{}-{nonce}", std::process::id()));
        match DirBuilder::new().mode(0o700).create(&path) {
            Ok(()) => {
                return path.canonicalize().map_err(|error| {
                    format!(
                        "failed to canonicalize task temp directory {}: {error}",
                        path.display()
                    )
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(format!(
                    "failed to create task temp directory {}: {error}",
                    path.display()
                ));
            }
        }
    }
    Err("failed to allocate a fresh task temp directory after 32 attempts".to_string())
}

pub(crate) fn is_managed_task_temp_dir(path: &Path) -> bool {
    path.file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| name.starts_with(".sandbox-tmp-"))
}

fn note_test_observation(
    ctx: &AppContext,
    principal: &AuthenticatedPrincipal,
    requested_tier: RequestedSandboxTier,
    task_kind: SandboxTaskKind,
) {
    let Some(observations) = TEST_OBSERVATIONS.get() else {
        return;
    };
    let Some(project_root) = ctx.config().project_root.clone() else {
        return;
    };
    let project_root = observation_key(&project_root);
    if let Some(project) = observations
        .lock()
        .expect("sandbox spawn test observation mutex poisoned")
        .get_mut(&project_root)
    {
        project.push(SandboxSpawnObservation {
            principal: principal.clone(),
            requested_tier,
            task_kind,
        });
    }
}

/// Start recording resolver calls for one project root.
#[doc(hidden)]
pub fn install_sandbox_spawn_test_seam(project_root: PathBuf) {
    TEST_OBSERVATIONS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .expect("sandbox spawn test observation mutex poisoned")
        .insert(observation_key(&project_root), Vec::new());
}

/// Snapshot resolver calls recorded for one project root.
#[doc(hidden)]
pub fn sandbox_spawn_test_observations(project_root: &Path) -> Vec<SandboxSpawnObservation> {
    TEST_OBSERVATIONS
        .get()
        .and_then(|observations| {
            observations
                .lock()
                .expect("sandbox spawn test observation mutex poisoned")
                .get(&observation_key(project_root))
                .cloned()
        })
        .unwrap_or_default()
}

/// Remove one project-root resolver test seam.
#[doc(hidden)]
pub fn clear_sandbox_spawn_test_seam(project_root: &Path) {
    if let Some(observations) = TEST_OBSERVATIONS.get() {
        observations
            .lock()
            .expect("sandbox spawn test observation mutex poisoned")
            .remove(&observation_key(project_root));
    }
}

fn observation_key(project_root: &Path) -> PathBuf {
    std::fs::canonicalize(project_root).unwrap_or_else(|_| project_root.to_path_buf())
}

#[cfg(test)]
pub(crate) fn with_spawn_plan_for_test<R>(plan: SpawnPlan, run: impl FnOnce() -> R) -> R {
    struct PlanScope(Option<SpawnPlan>);

    impl Drop for PlanScope {
        fn drop(&mut self) {
            TEST_PLAN_OVERRIDE.with(|slot| {
                slot.replace(self.0.take());
            });
        }
    }

    let previous = TEST_PLAN_OVERRIDE.with(|slot| slot.replace(Some(plan)));
    let _scope = PlanScope(previous);
    run()
}

/// Build a detached `Command` while enforcing the required launch plan.
///
/// Windows detached spawns route through the shell-candidate ladder, which
/// enforces the plan inline, so this helper is Unix-only.
#[cfg(unix)]
pub(crate) const CHILD_EXIT_FD: RawFd = 3;
#[cfg(unix)]
pub(crate) const CHILD_FAILURE_FD: RawFd = 4;
#[cfg(unix)]
pub(crate) const CHILD_PIPE_STATUS_FD: RawFd = 5;

#[cfg(unix)]
pub(crate) fn apply_marker_fd_allowlist(
    command: &mut Command,
    exit_fd: RawFd,
    failure_fd: RawFd,
    pipeline_status_fd: Option<RawFd>,
) -> Result<(RawFd, RawFd), String> {
    use std::os::unix::process::CommandExt;

    let fd_limit = unsafe { libc::sysconf(libc::_SC_OPEN_MAX) };
    let fd_limit = if fd_limit > 0 {
        (fd_limit as RawFd).min(65_536)
    } else {
        1_024
    };
    unsafe {
        command.pre_exec(move || {
            let exit_copy = libc::fcntl(exit_fd, libc::F_DUPFD_CLOEXEC, 6);
            if exit_copy < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let failure_copy = libc::fcntl(failure_fd, libc::F_DUPFD_CLOEXEC, 6);
            if failure_copy < 0 {
                let error = std::io::Error::last_os_error();
                libc::close(exit_copy);
                return Err(error);
            }
            let status_copy =
                pipeline_status_fd.map(|fd| libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 6));
            if status_copy.is_some_and(|fd| fd < 0) {
                let error = std::io::Error::last_os_error();
                libc::close(exit_copy);
                libc::close(failure_copy);
                return Err(error);
            }
            let status_copy = status_copy.unwrap_or(-1);
            if libc::dup2(exit_copy, CHILD_EXIT_FD) < 0
                || libc::dup2(failure_copy, CHILD_FAILURE_FD) < 0
                || (status_copy >= 0 && libc::dup2(status_copy, CHILD_PIPE_STATUS_FD) < 0)
            {
                let error = std::io::Error::last_os_error();
                libc::close(exit_copy);
                libc::close(failure_copy);
                if status_copy >= 0 {
                    libc::close(status_copy);
                }
                return Err(error);
            }
            libc::close(exit_copy);
            libc::close(failure_copy);
            if status_copy >= 0 {
                libc::close(status_copy);
            }
            let first_dynamic_fd = if pipeline_status_fd.is_some() {
                CHILD_PIPE_STATUS_FD + 1
            } else {
                CHILD_PIPE_STATUS_FD
            };
            for fd in first_dynamic_fd..fd_limit {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags >= 0 {
                    libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
                }
            }
            Ok(())
        });
    }
    Ok((CHILD_EXIT_FD, CHILD_FAILURE_FD))
}

#[cfg(unix)]
pub(crate) fn detached_command_for_plan(
    plan: &SpawnPlan,
    program: &OsStr,
    args: &[OsString],
    task_marker: &Path,
    exit_fd: RawFd,
    failure_fd: RawFd,
) -> Result<(Command, Option<File>), String> {
    let (program, args, profile_handle) = command_argv_for_plan(
        plan,
        program,
        args,
        task_marker,
        Some((exit_fd, failure_fd)),
    )?;
    let mut command = crate::effective_path::new_command(program);
    command.args(args);
    crate::bash_background::process::start_new_session(&mut command);
    Ok((command, profile_handle))
}

fn isolated_environment_for_plan(
    plan: &SpawnPlan,
    request_environment: &HashMap<String, String>,
) -> Option<ChildEnvironment> {
    #[cfg(unix)]
    if !matches!(plan.policy(), SpawnPlan::Unsandboxed) {
        if let Some(task) = plan.prepared_task() {
            return Some(task.environment().clone());
        }
    }
    match plan.policy() {
        SpawnPlan::Host { environment, .. } => Some(environment.clone()),
        SpawnPlan::Launcher { profile, .. } => Some(sandboxed_child_environment(
            request_environment,
            &profile.temp_dir,
        )),
        SpawnPlan::Unsandboxed | SpawnPlan::Refused { .. } => None,
        #[cfg(unix)]
        SpawnPlan::Prepared { .. } => unreachable!("policy() unwraps prepared plans"),
    }
}

fn sandboxed_child_environment(
    request_environment: &HashMap<String, String>,
    temp_dir: &Path,
) -> ChildEnvironment {
    let mut environment = std::env::vars_os()
        .filter(|(key, _)| sandbox_base_environment_key(key))
        .collect::<ChildEnvironment>();
    // PATH must be AFT's enriched value rather than the daemon's original
    // value, which can omit package-manager and user tool locations.
    environment.insert(
        OsString::from("PATH"),
        crate::effective_path::effective_path().to_os_string(),
    );
    for (key, value) in request_environment {
        // A request-supplied environment reaches the OUTER launcher / `/bin/sh`
        // supervisor, which exec before Landlock/Seatbelt is installed. A
        // dynamic-loader or shell/interpreter startup hook here would execute
        // code OUTSIDE confinement, so those keys are dropped even though they
        // arrived through the (otherwise honored) request environment.
        if is_preexec_hijack_env_key(key.as_str())
            || crate::agent_child_env::is_subc_credential_env_key(key)
            || key == crate::gh_shim_ticket::GH_SHIM_TICKET_ENV
        {
            continue;
        }
        environment.insert(OsString::from(key), OsString::from(value));
    }
    for key in ["TMPDIR", "TEMP", "TMP"] {
        environment.insert(OsString::from(key), temp_dir.as_os_str().to_os_string());
    }
    environment
}

fn sandbox_base_environment_key(key: &OsStr) -> bool {
    key.to_str().is_some_and(|key| {
        matches!(key, "HOME" | "USER" | "LOGNAME" | "SHELL" | "TERM" | "LANG")
            || key.starts_with("LC_")
    })
}

/// Environment keys that make a process load a library or run code during its
/// own startup — before a native sandbox is installed in the launcher chain.
/// These are refused from the request environment regardless of value so an
/// injected `LD_PRELOAD` / `DYLD_INSERT_LIBRARIES` / `BASH_ENV` cannot execute
/// outside confinement. Matching is case-sensitive; POSIX environment names are
/// case-sensitive and the loaders only honor the exact upper-case spellings.
fn is_preexec_hijack_env_key(key: &str) -> bool {
    // Dynamic-loader families (glibc/musl `LD_*`, macdyld `DYLD_*`): the whole
    // prefix executes/loads at exec time, so block the family, not a fixed set.
    if key.starts_with("LD_") || key.starts_with("DYLD_") {
        return true;
    }
    matches!(
        key,
        // POSIX/bash shell startup + trace hooks (the supervisor is `/bin/sh`).
        "BASH_ENV"
            | "ENV"
            | "SHELLOPTS"
            | "BASHOPTS"
            | "PROMPT_COMMAND"
            | "PS4"
            | "IFS"
            // Interpreter auto-run / library-injection hooks.
            | "PYTHONSTARTUP"
            | "PYTHONPATH"
            | "PYTHONHOME"
            | "PERL5OPT"
            | "PERL5LIB"
            | "PERLLIB"
            | "PERL5DB"
            | "RUBYOPT"
            | "RUBYLIB"
            | "NODE_OPTIONS"
            // glibc auxiliary loader hooks.
            | "GCONV_PATH"
            | "LOCPATH"
            | "NLSPATH"
            | "HOSTALIASES"
            | "RESOLV_HOST_CONF"
    )
}

#[cfg(unix)]
pub(crate) fn approved_environment_for_plan(
    plan: &SpawnPlan,
    request_environment: &HashMap<String, String>,
) -> ChildEnvironment {
    isolated_environment_for_plan(plan, request_environment)
        .unwrap_or_else(|| approved_payload_environment(request_environment, &std::env::temp_dir()))
}

#[cfg(unix)]
pub(crate) fn apply_sandbox_environment(
    plan: &SpawnPlan,
    command: &mut Command,
    request_environment: &HashMap<String, String>,
) {
    if let Some(environment) = isolated_environment_for_plan(plan, request_environment) {
        // Host snapshots and native-launcher allowlists are complete child
        // environments. Clear the daemon environment first so loader hooks,
        // shell startup hooks, and cloud credentials cannot leak around them.
        command.env_clear().envs(environment);
        if let Some(ticket) = memory_only_ticket(request_environment) {
            command.env(crate::gh_shim_ticket::GH_SHIM_TICKET_ENV, ticket);
        }
    }
}

/// The gh shim ticket is kept out of the isolated environment above because
/// that environment is written to the task's payload file on disk, and the
/// ticket must exist only in memory. It is added back here, at exec time.
fn memory_only_ticket(request_environment: &HashMap<String, String>) -> Option<&str> {
    request_environment
        .get(crate::gh_shim_ticket::GH_SHIM_TICKET_ENV)
        .map(String::as_str)
}

/// Build a PTY `CommandBuilder` while enforcing the required launch plan.
pub(crate) fn pty_command_for_plan(
    plan: &SpawnPlan,
    program: &OsStr,
    args: &[OsString],
    task_marker: &Path,
    workdir: &Path,
    env: &HashMap<String, String>,
) -> Result<(CommandBuilder, Option<File>), String> {
    let (program, args, profile_handle) =
        command_argv_for_plan(plan, program, args, task_marker, None)?;
    let mut command = CommandBuilder::new(program);
    for arg in args {
        command.arg(arg);
    }
    command.cwd(workdir.as_os_str());
    if let Some(environment) = isolated_environment_for_plan(plan, env) {
        command.env_clear();
        for (key, value) in environment {
            command.env(key, value);
        }
        if let Some(ticket) = memory_only_ticket(env) {
            command.env(crate::gh_shim_ticket::GH_SHIM_TICKET_ENV, ticket);
        }
    } else {
        // Sandbox-disabled PTYs retain the historical full inheritance and add
        // only request overrides. An inherited gh shim ticket is removed first:
        // it belongs to whatever started this process, and the request map
        // re-adds the child's own ticket when one was issued.
        command.env_remove(crate::gh_shim_ticket::GH_SHIM_TICKET_ENV);
        for (key, value) in env {
            command.env(key, value);
        }
    }
    crate::agent_child_env::scrub_pty_command(&mut command);
    Ok((command, profile_handle))
}

fn command_argv_for_plan(
    plan: &SpawnPlan,
    program: &OsStr,
    args: &[OsString],
    task_marker: &Path,
    marker_fds: Option<(RawFd, RawFd)>,
) -> Result<(OsString, Vec<OsString>, Option<File>), String> {
    match plan.policy() {
        SpawnPlan::Unsandboxed | SpawnPlan::Host { .. } => {
            Ok((program.to_os_string(), args.to_vec(), None))
        }
        SpawnPlan::Refused { code, .. } => Err((*code).to_string()),
        SpawnPlan::Launcher {
            profile,
            launcher_path,
        } => {
            #[cfg(unix)]
            {
                launcher_argv(
                    profile,
                    launcher_path,
                    program,
                    args,
                    task_marker,
                    marker_fds,
                    plan.prepared_task(),
                )
            }
            #[cfg(not(unix))]
            {
                launcher_argv(
                    profile,
                    launcher_path,
                    program,
                    args,
                    task_marker,
                    marker_fds,
                    None,
                )
            }
        }
        #[cfg(unix)]
        SpawnPlan::Prepared { .. } => unreachable!("policy() unwraps prepared plans"),
    }
}

#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
fn launcher_argv(
    profile: &SandboxProfile,
    launcher_path: &Path,
    program: &OsStr,
    args: &[OsString],
    _task_marker: &Path,
    marker_fds: Option<(RawFd, RawFd)>,
    _prepared: Option<&PreparedTask>,
) -> Result<(OsString, Vec<OsString>, Option<File>), String> {
    let profile_json = serde_json::to_string(profile)
        .map_err(|error| format!("failed to serialize sandbox profile: {error}"))?;
    let Some((exit_fd, failure_fd)) = marker_fds else {
        let mut wrapped = vec![
            OsString::from("sandbox-launch"),
            OsString::from("--profile-json"),
            OsString::from(profile_json),
            OsString::from("--"),
            program.to_os_string(),
        ];
        wrapped.extend_from_slice(args);
        return Ok((launcher_path.as_os_str().to_os_string(), wrapped, None));
    };

    let mut wrapped = vec![
        OsString::from("-c"),
        OsString::from(
            r#"launcher=$1
profile_json=$2
exit_fd=$3
failure_fd=$4
shift 4
"$launcher" sandbox-launch --profile-json "$profile_json" -- "$@"
code=$?
if [ "$code" -eq 78 ]; then
  printf "%s" sandbox_unavailable >&"$failure_fd"
  if [ ! -s "/dev/fd/$exit_fd" ]; then
    printf "%s" "$code" >&"$exit_fd"
  fi
fi
exit "$code""#,
        ),
        OsString::from("aft-sandbox-supervisor"),
        launcher_path.as_os_str().to_os_string(),
        OsString::from(profile_json),
        OsString::from(exit_fd.to_string()),
        OsString::from(failure_fd.to_string()),
        program.to_os_string(),
    ];
    wrapped.extend_from_slice(args);
    Ok((OsString::from("/bin/sh"), wrapped, None))
}

#[cfg(not(unix))]
#[allow(clippy::too_many_arguments)]
fn launcher_argv(
    _profile: &SandboxProfile,
    _launcher_path: &Path,
    _program: &OsStr,
    _args: &[OsString],
    _task_marker: &Path,
    _marker_fds: Option<(i32, i32)>,
    _prepared: Option<&()>,
) -> Result<(OsString, Vec<OsString>, Option<File>), String> {
    Err("sandbox_unavailable".to_string())
}

#[cfg(all(test, unix))]
mod tests {

    use super::*;

    #[test]
    fn host_plan_clears_inherited_environment_and_applies_snapshot() {
        let environment =
            ChildEnvironment::from([(OsString::from("APPROVED"), OsString::from("snapshot"))]);
        let plan = SpawnPlan::Host {
            shell_path: PathBuf::from("/bin/sh"),
            environment,
        };
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg("test -z \"$SHOULD_DISAPPEAR\" && printf %s \"$APPROVED\"")
            .env("SHOULD_DISAPPEAR", "yes");
        apply_sandbox_environment(&plan, &mut command, &HashMap::new());
        let output = command.output().unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"snapshot");
    }

    #[test]
    fn launcher_plan_clears_ambient_environment_and_applies_safe_base() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let temp = root.path().join("temp");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&temp).unwrap();
        let profile = SandboxProfile::build(
            vec![project],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            temp,
        )
        .unwrap();
        let expected_temp = profile.temp_dir.clone();
        let plan = SpawnPlan::launcher_for_test(profile, PathBuf::from("/usr/bin/true"));
        let request_environment = HashMap::from([
            ("TERM".to_string(), "aft-test-term".to_string()),
            ("REQUEST_SENTINEL".to_string(), "request-value".to_string()),
        ]);
        let mut command = Command::new("/usr/bin/env");
        command
            .env("LD_PRELOAD", "/untrusted/loader.so")
            .env("DYLD_INSERT_LIBRARIES", "/untrusted/loader.dylib")
            .env("BASH_ENV", "/untrusted/bash-env")
            .env("AWS_SECRET_ACCESS_KEY", "ambient-secret");

        apply_sandbox_environment(&plan, &mut command, &request_environment);
        let output = command.output().unwrap();
        assert!(output.status.success());
        let output = String::from_utf8(output.stdout).unwrap();

        for leaked in [
            "LD_PRELOAD=",
            "DYLD_INSERT_LIBRARIES=",
            "BASH_ENV=",
            "AWS_SECRET_ACCESS_KEY=",
        ] {
            assert!(
                !output.contains(leaked),
                "ambient variable leaked: {leaked}"
            );
        }
        assert!(output.contains("REQUEST_SENTINEL=request-value\n"));
        assert!(output.contains("TERM=aft-test-term\n"));
        assert!(output.contains(&format!(
            "PATH={}\n",
            crate::effective_path::effective_path().to_string_lossy()
        )));
        if let Some(home) = std::env::var_os("HOME") {
            assert!(output.contains(&format!("HOME={}\n", home.to_string_lossy())));
        }
        for key in ["TMPDIR", "TEMP", "TMP"] {
            assert!(output.contains(&format!("{key}={}\n", expected_temp.display())));
        }
    }

    #[cfg(unix)]
    #[test]
    fn request_environment_cannot_inject_preexec_hijack_variables() {
        // The pre-sandbox boundary: a request-supplied environment is applied to
        // the OUTER launcher / `/bin/sh` supervisor, which run BEFORE Landlock or
        // Seatbelt is installed. A loader/shell/interpreter startup hook arriving
        // through the request env must be dropped, or injected code executes
        // outside confinement. (The sibling test covers the ambient direction;
        // this covers the request-supplied direction the re-audit flagged.)
        let project = tempfile::tempdir().unwrap();
        let temp = project.path().join("sandbox-temp");
        std::fs::create_dir(&temp).unwrap();
        let profile = crate::sandbox_profile::SandboxProfile::build(
            vec![project.path().to_path_buf()],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            temp,
        )
        .unwrap();
        let plan = SpawnPlan::launcher_for_test(profile, PathBuf::from("/usr/bin/true"));

        // Every dangerous key arrives through the REQUEST environment this time.
        let hijacks = [
            ("LD_PRELOAD", "/untrusted/loader.so"),
            ("LD_LIBRARY_PATH", "/untrusted/lib"),
            ("LD_AUDIT", "/untrusted/audit.so"),
            ("DYLD_INSERT_LIBRARIES", "/untrusted/loader.dylib"),
            ("DYLD_LIBRARY_PATH", "/untrusted/dylib"),
            ("BASH_ENV", "/untrusted/bash-env"),
            ("ENV", "/untrusted/sh-env"),
            ("PROMPT_COMMAND", "/untrusted/cmd"),
            ("PS4", "evil"),
            ("PYTHONSTARTUP", "/untrusted/py"),
            ("PYTHONPATH", "/untrusted/pypath"),
            ("PERL5OPT", "-M/untrusted"),
            ("RUBYOPT", "-r/untrusted"),
            ("NODE_OPTIONS", "--require=/untrusted"),
            ("GCONV_PATH", "/untrusted/gconv"),
            ("LOCPATH", "/untrusted/loc"),
        ];
        let request_environment: HashMap<String, String> = hijacks
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .chain([("REQUEST_SENTINEL".to_string(), "kept".to_string())])
            .collect();

        let mut command = Command::new("/usr/bin/env");
        apply_sandbox_environment(&plan, &mut command, &request_environment);
        let output = command.output().unwrap();
        assert!(output.status.success());
        let output = String::from_utf8(output.stdout).unwrap();

        for (key, _) in hijacks {
            assert!(
                !output.contains(&format!("{key}=")),
                "request-supplied hijack var reached the pre-sandbox child: {key}\n{output}"
            );
        }
        // A benign request var still flows through, proving we filtered rather
        // than dropped the whole request environment.
        assert!(output.contains("REQUEST_SENTINEL=kept\n"), "{output}");
    }

    #[test]
    fn request_environment_cannot_smuggle_subc_credentials_into_sandbox_launcher() {
        let temp = tempfile::tempdir().unwrap();
        let request_environment = HashMap::from([
            ("SUBC_MODULE_ID".to_string(), "aft".to_string()),
            ("SUBC_LAUNCH_NONCE".to_string(), "nonce".to_string()),
            (
                "SUBC_FUTURE_CREDENTIAL".to_string(),
                "future-secret".to_string(),
            ),
            ("REQUEST_SENTINEL".to_string(), "kept".to_string()),
        ]);

        let environment = sandboxed_child_environment(&request_environment, temp.path());

        assert!(environment
            .keys()
            .filter_map(|key| key.to_str())
            .all(|key| !crate::agent_child_env::is_subc_credential_env_key(key)));
        assert_eq!(
            environment.get(OsStr::new("REQUEST_SENTINEL")),
            Some(&OsString::from("kept"))
        );
    }

    #[test]
    fn unsandboxed_plan_preserves_inherited_and_request_environment() {
        let plan = SpawnPlan::Unsandboxed;
        let request_environment =
            HashMap::from([("REQUEST_SENTINEL".to_string(), "request-value".to_string())]);
        #[cfg(target_os = "macos")]
        let loader_hook = "/usr/lib/libSystem.B.dylib";
        #[cfg(target_os = "linux")]
        let loader_hook = "libc.so.6";
        let mut command = Command::new("/usr/bin/env");
        command
            .env("UNSANDBOXED_PARENT_SENTINEL", "raw-parent")
            .env("LD_PRELOAD", loader_hook)
            .env("DYLD_INSERT_LIBRARIES", loader_hook)
            .env("AWS_SECRET_ACCESS_KEY", "ambient-cloud-secret")
            .envs(&request_environment);

        let before = command
            .get_envs()
            .map(|(key, value)| (key.to_os_string(), value.map(OsStr::to_os_string)))
            .collect::<Vec<_>>();
        apply_sandbox_environment(&plan, &mut command, &request_environment);
        let after = command
            .get_envs()
            .map(|(key, value)| (key.to_os_string(), value.map(OsStr::to_os_string)))
            .collect::<Vec<_>>();
        assert_eq!(after, before, "unsandboxed environment overrides changed");
        let output = command.output().unwrap();
        assert!(output.status.success());
        let output = String::from_utf8(output.stdout).unwrap();
        assert!(output.contains("UNSANDBOXED_PARENT_SENTINEL=raw-parent\n"));
        assert!(output.contains(&format!("LD_PRELOAD={loader_hook}\n")));
        #[cfg(target_os = "linux")]
        assert!(output.contains(&format!("DYLD_INSERT_LIBRARIES={loader_hook}\n")));
        assert!(output.contains("AWS_SECRET_ACCESS_KEY=ambient-cloud-secret\n"));
        assert!(output.contains("REQUEST_SENTINEL=request-value\n"));
    }

    #[test]
    fn product_profile_is_passed_as_a_verified_buffer() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let temp = root.path().join("temp");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&temp).unwrap();
        let profile = SandboxProfile::build(
            vec![project],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            temp,
        )
        .unwrap();
        let (_program, args, retained) = launcher_argv(
            &profile,
            Path::new("/bin/aft"),
            OsStr::new("/bin/sh"),
            &[OsString::from("-c"), OsString::from("true")],
            Path::new("unused"),
            None,
            None,
        )
        .unwrap();
        assert_eq!(args[0], "sandbox-launch");
        assert_eq!(args[1], "--profile-json");
        assert!(serde_json::from_str::<SandboxProfile>(args[2].to_str().unwrap()).is_ok());
        assert!(retained.is_none());
        assert!(!root.path().join("sandbox-profile.json").exists());
    }
}

#[cfg(test)]
mod policy_tests {
    use super::*;

    fn context(project_root: PathBuf) -> AppContext {
        AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                project_root: Some(project_root),
                sandbox: crate::config::SandboxConfig {
                    enabled: true,
                    ..crate::config::SandboxConfig::default()
                },
                ..crate::config::Config::default()
            },
        )
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn concurrent_sandbox_marker_pty_and_git_spawns_preserve_fd_ownership() {
        use portable_pty::{CommandBuilder, PtySize};
        use std::fs::OpenOptions;
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::OpenOptionsExt;
        use std::process::Stdio;
        use std::sync::{Arc, Barrier};

        const ITERATIONS: usize = 32;
        let fixture = tempfile::tempdir().unwrap();
        let project = fixture.path().join("project");
        let task_io = fixture
            .path()
            .join("tasks/session/bash-0000000000000001/io");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&task_io).unwrap();
        let initialized = Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&project)
            .status()
            .unwrap();
        assert!(initialized.success());

        let ctx = Arc::new(context(project));
        let task_io = Arc::new(task_io);
        let barrier = Arc::new(Barrier::new(4));

        let resolver = {
            let ctx = Arc::clone(&ctx);
            let task_io = Arc::clone(&task_io);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                for _ in 0..ITERATIONS {
                    let plan = resolve_sandbox_spawn(
                        &ctx,
                        &AuthenticatedPrincipal::FirstParty,
                        RequestedSandboxTier::Native,
                        SandboxTaskKind::BashBackground,
                        &task_io,
                        None,
                    );
                    assert!(plan.is_native_launcher(), "unexpected plan: {plan:?}");
                    plan.cleanup_unspawned();
                }
            })
        };
        let detached = {
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                for _ in 0..ITERATIONS {
                    let exit = tempfile::tempfile().unwrap();
                    let failure = tempfile::tempfile().unwrap();
                    crate::bash_background::persistence::set_close_on_exec(exit.as_raw_fd(), false)
                        .unwrap();
                    crate::bash_background::persistence::set_close_on_exec(
                        failure.as_raw_fd(),
                        false,
                    )
                    .unwrap();
                    let mut command = Command::new("/usr/bin/true");
                    apply_marker_fd_allowlist(
                        &mut command,
                        exit.as_raw_fd(),
                        failure.as_raw_fd(),
                        None,
                    )
                    .unwrap();
                    let status = command
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status()
                        .unwrap();
                    assert!(status.success());
                }
            })
        };
        let pty = {
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                for _ in 0..ITERATIONS {
                    let pair = portable_pty::native_pty_system()
                        .openpty(PtySize {
                            rows: 24,
                            cols: 80,
                            pixel_width: 0,
                            pixel_height: 0,
                        })
                        .unwrap();
                    let mut child = pair
                        .slave
                        .spawn_command(CommandBuilder::new("/usr/bin/true"))
                        .unwrap();
                    child.wait().unwrap();
                }
            })
        };
        let probes = {
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                for _ in 0..ITERATIONS {
                    let output = Command::new("git").arg("--version").output().unwrap();
                    assert!(output.status.success());
                    let file = OpenOptions::new()
                        .read(true)
                        .write(true)
                        .custom_flags(libc::O_CLOEXEC)
                        .open("/dev/null")
                        .unwrap();
                    drop(file);
                }
            })
        };

        resolver.join().unwrap();
        detached.join().unwrap();
        pty.join().unwrap();
        probes.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn sandbox_setup_refusal_names_root_and_cause_before_remedy() {
        let project = tempfile::tempdir().unwrap();
        let ctx = context(project.path().to_path_buf());
        let plan = crate::log_ctx::with_session(Some("fd-audit".to_string()), || {
            sandbox_setup_refusal(
                &ctx,
                &AuthenticatedPrincipal::FirstParty,
                "failed to query core.hooksPath: Bad file descriptor (os error 9)",
            )
        });
        let message = plan.refusal_message().expect("sandbox refusal message");
        assert!(message.starts_with(&format!(
            "sandbox setup for {} failed: failed to query core.hooksPath: Bad file descriptor (os error 9)",
            project.path().display()
        )));
        assert!(message.ends_with("set sandbox.enabled=false to disable native sandboxing"));
    }

    #[cfg(unix)]
    #[test]
    fn native_sandbox_predicate_controls_spawn_and_rewrite() {
        let project = tempfile::tempdir().unwrap();
        let file = project.path().join("rewrite-probe.txt");
        std::fs::write(&file, "sandboxed\n").unwrap();
        let ctx = context(project.path().to_path_buf());
        ctx.update_config(|config| config.experimental_bash_rewrite = true);
        let principal = AuthenticatedPrincipal::FirstParty;
        let command = format!("cat {}", file.display());

        assert!(native_sandbox_enforced(&ctx, &principal));
        assert!(crate::bash_rewrite::try_rewrite(&command, None, &ctx, &principal).is_none());
        let sandboxed = resolve_sandbox_spawn(
            &ctx,
            &principal,
            RequestedSandboxTier::Native,
            SandboxTaskKind::BashForeground,
            project.path(),
            None,
        );
        assert!(matches!(&sandboxed, SpawnPlan::Launcher { .. }));
        sandboxed.cleanup_unspawned();

        ctx.update_config(|config| config.sandbox.enabled = false);
        assert!(!native_sandbox_enforced(&ctx, &principal));
        assert!(crate::bash_rewrite::try_rewrite(&command, None, &ctx, &principal).is_some());
        assert_eq!(
            resolve_sandbox_spawn(
                &ctx,
                &principal,
                RequestedSandboxTier::Native,
                SandboxTaskKind::BashForeground,
                project.path(),
                None,
            ),
            SpawnPlan::Unsandboxed
        );
    }

    #[test]
    fn untrusted_principal_never_enters_the_native_launcher() {
        let project = tempfile::tempdir().unwrap();
        let ctx = context(project.path().to_path_buf());
        let principal = AuthenticatedPrincipal::RouteBind {
            trust: PrincipalTrust::Untrusted,
            route_channel: 7,
            route_epoch: 1,
            project_root: project.path().to_path_buf(),
            harness: "mcp:test".to_string(),
            session_id: "untrusted-sandbox-test".to_string(),
            principal_id: Some("unverified".to_string()),
        };

        let plan = resolve_sandbox_spawn(
            &ctx,
            &principal,
            RequestedSandboxTier::Native,
            SandboxTaskKind::BashForeground,
            project.path(),
            None,
        );
        // The invariant is that an untrusted principal never receives a native
        // Launcher plan. On Unix that surfaces as a downgrade to Unsandboxed
        // (the untrusted principal fails the first-party enforcement check); on
        // platforms without a kernel backend the enabled policy fails closed
        // with a platform refusal before the trust check is reached. Both honor
        // the invariant, so assert the exact non-Launcher outcome per platform.
        #[cfg(unix)]
        assert_eq!(plan, SpawnPlan::Unsandboxed);
        #[cfg(not(unix))]
        assert!(
            matches!(&plan, SpawnPlan::Refused { code, .. } if *code == "sandbox_unavailable"),
            "untrusted principal must never reach the native launcher; got {plan:?}"
        );
    }

    #[cfg(unix)]
    fn grant_attempt(
        grant_id: String,
        project: &Path,
        command: &[u8],
        environment: ChildEnvironment,
    ) -> HostEscalationAttempt {
        HostEscalationAttempt {
            grant_id,
            command: command.to_vec(),
            root: project.to_path_buf(),
            cwd: project.to_path_buf(),
            shell_path: PathBuf::from("/bin/sh"),
            environment,
        }
    }

    #[cfg(unix)]
    fn mint_test_grant(
        ctx: &AppContext,
        principal: &AuthenticatedPrincipal,
        project: &Path,
        command: &[u8],
        environment: &ChildEnvironment,
        now: Instant,
    ) -> String {
        mint_host_escalation_grant_at(
            ctx,
            principal,
            command,
            project,
            project,
            Path::new("/bin/sh"),
            environment,
            &project.join(".aft-test-storage"),
            "test-session",
            now,
        )
        .unwrap()
    }

    #[cfg(unix)]
    fn refusal_class(plan: &SpawnPlan) -> Option<&'static str> {
        plan.refusal_mismatch_class()
    }

    #[cfg(unix)]
    #[test]
    fn escalation_grant_binds_exact_command_and_environment() {
        let project = tempfile::tempdir().unwrap();
        let ctx = context(project.path().to_path_buf());
        let principal = AuthenticatedPrincipal::FirstParty;
        let environment = ChildEnvironment::from([
            (OsString::from("A"), OsString::from("one")),
            (OsString::from("B"), OsString::from("two")),
        ]);

        for (command, retry_environment) in [
            (b"printf approved!".as_slice(), environment.clone()),
            (
                b"printf approved".as_slice(),
                ChildEnvironment::from([
                    (OsString::from("A"), OsString::from("changed")),
                    (OsString::from("B"), OsString::from("two")),
                ]),
            ),
        ] {
            let grant_id = mint_test_grant(
                &ctx,
                &principal,
                project.path(),
                b"printf approved",
                &environment,
                Instant::now(),
            );
            let attempt = grant_attempt(grant_id, project.path(), command, retry_environment);
            let plan = resolve_sandbox_spawn(
                &ctx,
                &principal,
                RequestedSandboxTier::Host,
                SandboxTaskKind::BashForeground,
                project.path(),
                Some(&attempt),
            );
            assert_eq!(refusal_class(&plan), Some("digest_mismatch"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn escalation_grant_is_single_use_and_expires() {
        let project = tempfile::tempdir().unwrap();
        let ctx = context(project.path().to_path_buf());
        let principal = AuthenticatedPrincipal::FirstParty;
        let environment =
            ChildEnvironment::from([(OsString::from("ONLY"), OsString::from("snapshot"))]);
        let grant_id = mint_test_grant(
            &ctx,
            &principal,
            project.path(),
            b"true",
            &environment,
            Instant::now(),
        );
        let attempt = grant_attempt(grant_id, project.path(), b"true", environment.clone());
        let first = resolve_sandbox_spawn(
            &ctx,
            &principal,
            RequestedSandboxTier::Host,
            SandboxTaskKind::BashForeground,
            project.path(),
            Some(&attempt),
        );
        assert!(matches!(first.policy(), SpawnPlan::Host { .. }));
        let second = resolve_sandbox_spawn(
            &ctx,
            &principal,
            RequestedSandboxTier::Host,
            SandboxTaskKind::BashForeground,
            project.path(),
            Some(&attempt),
        );
        assert_eq!(refusal_class(&second), Some("consumed"));

        let expired_id = mint_test_grant(
            &ctx,
            &principal,
            project.path(),
            b"true",
            &environment,
            Instant::now() - ESCALATION_GRANT_TTL - Duration::from_millis(1),
        );
        let expired_attempt =
            grant_attempt(expired_id, project.path(), b"true", environment.clone());
        let expired = resolve_sandbox_spawn(
            &ctx,
            &principal,
            RequestedSandboxTier::Host,
            SandboxTaskKind::BashForeground,
            project.path(),
            Some(&expired_attempt),
        );
        assert_eq!(refusal_class(&expired), Some("expired"));
    }

    #[cfg(unix)]
    #[test]
    fn escalation_grant_binds_principal_and_happy_path_reuses_snapshot() {
        let project = tempfile::tempdir().unwrap();
        let ctx = context(project.path().to_path_buf());
        let principal = AuthenticatedPrincipal::RouteBind {
            trust: PrincipalTrust::FirstParty,
            route_channel: 9,
            route_epoch: 2,
            project_root: project.path().to_path_buf(),
            harness: "opencode".to_string(),
            session_id: "session-x".to_string(),
            principal_id: Some("direct".to_string()),
        };
        let environment =
            ChildEnvironment::from([(OsString::from("APPROVED"), OsString::from("snapshot"))]);
        let wrong_id = mint_test_grant(
            &ctx,
            &principal,
            project.path(),
            b"true",
            &environment,
            Instant::now(),
        );
        let wrong_attempt = grant_attempt(wrong_id, project.path(), b"true", environment.clone());
        let wrong = resolve_sandbox_spawn(
            &ctx,
            &AuthenticatedPrincipal::FirstParty,
            RequestedSandboxTier::Host,
            SandboxTaskKind::BashForeground,
            project.path(),
            Some(&wrong_attempt),
        );
        assert_eq!(refusal_class(&wrong), Some("wrong_principal"));

        let happy_id = mint_test_grant(
            &ctx,
            &principal,
            project.path(),
            b"true",
            &environment,
            Instant::now(),
        );
        let happy_attempt = grant_attempt(happy_id, project.path(), b"true", environment.clone());
        let happy = resolve_sandbox_spawn(
            &ctx,
            &principal,
            RequestedSandboxTier::Host,
            SandboxTaskKind::BashForeground,
            project.path(),
            Some(&happy_attempt),
        );
        match happy.policy() {
            SpawnPlan::Host {
                shell_path,
                environment: actual,
            } => {
                assert_eq!(shell_path, Path::new("/bin/sh"));
                assert_eq!(actual, &environment);
            }
            other => panic!("expected host plan, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn untrusted_host_request_refuses_without_minting_a_grant() {
        let project = tempfile::tempdir().unwrap();
        let ctx = context(project.path().to_path_buf());
        let principal = AuthenticatedPrincipal::RouteBind {
            trust: PrincipalTrust::Untrusted,
            route_channel: 7,
            route_epoch: 1,
            project_root: project.path().to_path_buf(),
            harness: "mcp:test".to_string(),
            session_id: "untrusted-escalation-test".to_string(),
            principal_id: Some("unverified".to_string()),
        };
        let plan = resolve_sandbox_spawn(
            &ctx,
            &principal,
            RequestedSandboxTier::Host,
            SandboxTaskKind::BashForeground,
            project.path(),
            None,
        );
        assert_eq!(plan.refusal_code(), Some("sandbox_escalation_denied"));
        assert!(ctx.escalation_grants().lock().grants.is_empty());
    }

    #[cfg(unix)]
    fn grant_task_paths(ctx: &AppContext, grant_id: &str) -> (PathBuf, String) {
        let store = ctx.escalation_grants().lock();
        let grant = store.grants.get(grant_id).unwrap();
        (grant.session_dir.clone(), grant.task_id.clone())
    }

    #[cfg(unix)]
    #[test]
    fn escalated_payload_path_race_is_refused_and_burns_the_grant() {
        use std::os::unix::fs::symlink;

        let project = tempfile::tempdir().unwrap();
        let ctx = context(project.path().to_path_buf());
        let principal = AuthenticatedPrincipal::FirstParty;
        let environment = ChildEnvironment::new();
        let grant_id = mint_test_grant(
            &ctx,
            &principal,
            project.path(),
            b"true",
            &environment,
            Instant::now(),
        );
        let (session_dir, task_id) = grant_task_paths(&ctx, &grant_id);
        let command_path = session_dir
            .join(&task_id)
            .join("control")
            .join(crate::bash_background::persistence::COMMAND_FILE);
        let victim = project.path().join("victim");
        std::fs::write(&victim, b"victim-bytes").unwrap();
        std::fs::remove_file(&command_path).unwrap();
        symlink(&victim, &command_path).unwrap();

        let attempt = grant_attempt(grant_id.clone(), project.path(), b"true", environment);
        let refused = resolve_sandbox_spawn(
            &ctx,
            &principal,
            RequestedSandboxTier::Host,
            SandboxTaskKind::BashForeground,
            project.path(),
            Some(&attempt),
        );
        assert_eq!(refusal_class(&refused), Some("digest_mismatch"));
        assert_eq!(std::fs::read(&victim).unwrap(), b"victim-bytes");
        let consumed = resolve_sandbox_spawn(
            &ctx,
            &principal,
            RequestedSandboxTier::Host,
            SandboxTaskKind::BashForeground,
            project.path(),
            Some(&attempt),
        );
        assert_eq!(refusal_class(&consumed), Some("consumed"));
    }

    #[cfg(unix)]
    #[test]
    fn verified_host_payload_executes_verified_buffers_after_inode_mutation() {
        use std::fs::OpenOptions;
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::OpenOptionsExt;

        let project = tempfile::tempdir().unwrap();
        let ctx = context(project.path().to_path_buf());
        let principal = AuthenticatedPrincipal::FirstParty;
        let environment = ChildEnvironment::new();
        let grant_id = mint_test_grant(
            &ctx,
            &principal,
            project.path(),
            b"true",
            &environment,
            Instant::now(),
        );
        let attempt = grant_attempt(grant_id, project.path(), b"true", environment);
        let plan = resolve_sandbox_spawn(
            &ctx,
            &principal,
            RequestedSandboxTier::Host,
            SandboxTaskKind::BashForeground,
            project.path(),
            Some(&attempt),
        );
        let prepared = plan.prepared_task().expect("verified prepared task");
        let command_path = prepared
            .paths()
            .control_dir
            .join(crate::bash_background::persistence::COMMAND_FILE);
        let victim = project.path().join("victim");
        std::fs::write(&victim, b"victim-bytes").unwrap();
        let payload = prepared.invocation().unwrap();
        // Mutate the same inode after verification. Execution must use the
        // verified in-memory buffers rather than rereading the held file.
        std::fs::write(
            &command_path,
            format!("printf hacked > {}", victim.display()),
        )
        .unwrap();
        let exit_path = project.path().join("exit");
        let exit = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&exit_path)
            .unwrap();
        crate::bash_background::persistence::set_close_on_exec(exit.as_raw_fd(), false).unwrap();
        // Mirror production (apply_marker_fd_allowlist): the real marker fd is
        // dup2'd onto the low, single-digit CHILD_EXIT_FD before exec, and the
        // wrapper is handed that literal. POSIX sh (dash) only parses a
        // single-digit `>&N` redirect target, so passing a raw multi-digit fd
        // here would diverge from production and fail under dash with
        // "Bad fd number" (production never hits this: it always remaps to 3).
        let raw_exit_fd = exit.as_raw_fd();
        let exit_fd = CHILD_EXIT_FD.to_string();
        let mut command = Command::new("/bin/sh");
        command.args([
            OsStr::new("-c"),
            payload.wrapper_text.as_os_str(),
            OsStr::new("aft-payload-wrapper"),
            OsStr::new("/bin/sh"),
            payload.command_text.as_os_str(),
            OsStr::new(&exit_fd),
        ]);
        {
            use std::os::unix::process::CommandExt;
            unsafe {
                command.pre_exec(move || {
                    if raw_exit_fd != CHILD_EXIT_FD && libc::dup2(raw_exit_fd, CHILD_EXIT_FD) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        let status = command.status().unwrap();
        assert!(status.success());
        assert_eq!(std::fs::read(&victim).unwrap(), b"victim-bytes");
    }

    #[cfg(unix)]
    #[test]
    fn approval_spawn_drift_matrix_refuses_every_bound_field() {
        let project = tempfile::tempdir().unwrap();
        let ctx = context(project.path().to_path_buf());
        let principal = AuthenticatedPrincipal::FirstParty;
        let approved = ChildEnvironment::from([(OsString::from("A"), OsString::from("one"))]);
        for drift in [
            "command",
            "newline",
            "encoding",
            "cwd",
            "root",
            "shell",
            "environment",
            "environment_file_encoding",
            "wrapper_template",
        ] {
            let grant_id = mint_test_grant(
                &ctx,
                &principal,
                project.path(),
                b"printf approved",
                &approved,
                Instant::now(),
            );
            let mut attempt = grant_attempt(
                grant_id,
                project.path(),
                b"printf approved",
                approved.clone(),
            );
            match drift {
                "command" => attempt.command = b"printf changed".to_vec(),
                "newline" => attempt.command.push(b'\n'),
                "encoding" => attempt.command.push(0xff),
                "cwd" => attempt.cwd = project.path().join("changed-cwd"),
                "root" => attempt.root = project.path().join("changed-root"),
                "shell" => attempt.shell_path = PathBuf::from("/bin/bash"),
                "environment" => {
                    attempt
                        .environment
                        .insert(OsString::from("A"), OsString::from("two"));
                }
                "environment_file_encoding" | "wrapper_template" => {
                    let (session_dir, task_id) = grant_task_paths(&ctx, &attempt.grant_id);
                    let name = if drift == "wrapper_template" {
                        crate::bash_background::persistence::WRAPPER_FILE
                    } else {
                        crate::bash_background::persistence::ENVIRONMENT_FILE
                    };
                    std::fs::write(
                        session_dir.join(task_id).join("control").join(name),
                        b"drift",
                    )
                    .unwrap();
                }
                _ => unreachable!(),
            }
            let refused = resolve_sandbox_spawn(
                &ctx,
                &principal,
                RequestedSandboxTier::Host,
                SandboxTaskKind::BashForeground,
                project.path(),
                Some(&attempt),
            );
            assert_eq!(refusal_class(&refused), Some("digest_mismatch"), "{drift}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn payload_read_grant_seam_exposes_only_exact_control_objects() {
        let storage = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let principal = AuthenticatedPrincipal::FirstParty;
        let environment = ChildEnvironment::from([(OsString::from("SAFE"), OsString::from("yes"))]);
        let layout =
            crate::bash_background::persistence::allocate_task_layout(storage.path(), "session")
                .unwrap();
        let task = prepare_task_payload(
            &layout,
            b"true",
            project.path(),
            project.path(),
            &principal,
            Path::new("/bin/sh"),
            &environment,
        )
        .unwrap();
        let plan = SpawnPlan::Unsandboxed.with_prepared_task(task.clone());
        let grants = plan.payload_read_grants();
        assert_eq!(grants.len(), 3);
        assert_eq!(grants, task.payload_read_grants());
        assert!(grants
            .iter()
            .all(|path| path.parent() == Some(task.paths().control_dir.as_path())));
        assert!(!grants.contains(&task.paths().manifest));
        let sandbox_temp = storage.path().join("sandbox-temp");
        std::fs::create_dir_all(&sandbox_temp).unwrap();
        let b2_profile = SandboxProfile::build(
            vec![project.path().to_path_buf()],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            vec![task.paths().control_dir.clone()],
            Vec::new(),
            Vec::new(),
            sandbox_temp,
        )
        .unwrap();
        assert!(b2_profile
            .read_deny
            .contains(&std::fs::canonicalize(&task.paths().control_dir).unwrap()));
        assert!(grants.iter().all(|path| {
            path.parent() == Some(task.paths().control_dir.as_path())
                && path != &task.paths().manifest
        }));

        #[cfg(target_os = "linux")]
        {
            let refused = SpawnPlan::launcher_for_test(b2_profile, PathBuf::from("/usr/bin/false"))
                .with_prepared_task(task.clone());
            assert_eq!(refused.refusal_code(), Some("sandbox_unavailable"));
            assert!(refused
                .refusal_message()
                .is_some_and(|message| message.contains("mandatory read root")));

            let allowed_temp = storage.path().join("allowed-sandbox-temp");
            std::fs::create_dir_all(&allowed_temp).unwrap();
            let allowed_profile = SandboxProfile::build(
                vec![project.path().to_path_buf()],
                Vec::new(),
                Vec::new(),
                vec![project.path().to_path_buf()],
                Vec::new(),
                Vec::new(),
                Vec::new(),
                allowed_temp,
            )
            .unwrap();
            let prepared =
                SpawnPlan::launcher_for_test(allowed_profile, PathBuf::from("/usr/bin/true"))
                    .with_prepared_task(task.clone());
            let profile = match prepared {
                SpawnPlan::Prepared { plan, .. } => match *plan {
                    SpawnPlan::Launcher { profile, .. } => profile,
                    other => panic!("expected launcher plan, got {other:?}"),
                },
                other => panic!("expected prepared plan, got {other:?}"),
            };
            let canonical_payloads = grants
                .iter()
                .map(|path| path.canonicalize().unwrap())
                .collect::<Vec<_>>();
            assert!(canonical_payloads
                .iter()
                .all(|path| profile.read_allow.contains(path)));
            assert!(profile.read_allow.iter().all(|path| {
                !path.starts_with(&task.paths().control_dir) || canonical_payloads.contains(path)
            }));
            validate_final_read_rules(&profile.read_allow, &profile.read_deny).unwrap();
        }

        assert_eq!(task.environment(), &environment);
    }

    #[cfg(unix)]
    #[test]
    fn writable_roots_refuse_both_session_store_overlap_directions() {
        fn profile(base: &Path, write_root: PathBuf) -> SandboxProfile {
            let project = base.join("project");
            let home = base.join("home");
            let temp = base.join("temp");
            for path in [&project, &home, &temp, &write_root] {
                std::fs::create_dir_all(path).unwrap();
            }
            SandboxProfile::build(
                vec![project, write_root],
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                temp,
            )
            .unwrap()
        }

        let base = tempfile::tempdir().unwrap();
        let session = base.path().join("store/session");
        let io = session.join("bash-0000000000000001/io");
        let control = session.join("bash-0000000000000001/control");
        std::fs::create_dir_all(&io).unwrap();
        std::fs::create_dir_all(&control).unwrap();
        let canonical_session = std::fs::canonicalize(&session).unwrap();
        let canonical_io = std::fs::canonicalize(&io).unwrap();

        let ancestor = profile(base.path(), base.path().join("store"));
        assert!(refuse_store_overlap(&ancestor, &canonical_session, &canonical_io).is_err());
        let descendant = profile(base.path(), control);
        assert!(refuse_store_overlap(&descendant, &canonical_session, &canonical_io).is_err());
        let allowed = profile(base.path(), io);
        assert!(refuse_store_overlap(&allowed, &canonical_session, &canonical_io).is_ok());
    }

    #[cfg(windows)]
    #[test]
    fn enabled_host_request_is_refused_on_windows_without_a_grant() {
        let project = tempfile::tempdir().unwrap();
        let ctx = context(project.path().to_path_buf());
        let plan = resolve_sandbox_spawn(
            &ctx,
            &AuthenticatedPrincipal::FirstParty,
            RequestedSandboxTier::Host,
            SandboxTaskKind::BashForeground,
            project.path(),
            None,
        );
        assert_eq!(plan.refusal_code(), Some("sandbox_unavailable"));
        assert_eq!(
            plan.refusal_message(),
            Some(
                "sandbox is not supported on this platform; disable sandbox.enabled or run on macOS/Linux"
            )
        );
    }

    #[cfg(windows)]
    #[test]
    fn enabled_native_tier_is_refused_on_windows() {
        let project = tempfile::tempdir().unwrap();
        let ctx = context(project.path().to_path_buf());
        let plan = resolve_sandbox_spawn(
            &ctx,
            &AuthenticatedPrincipal::FirstParty,
            RequestedSandboxTier::Native,
            SandboxTaskKind::BashForeground,
            project.path(),
            None,
        );
        assert_eq!(plan.refusal_code(), Some("sandbox_unavailable"));
        assert_eq!(
            plan.refusal_message(),
            Some(
                "sandbox is not supported on this platform; disable sandbox.enabled or run on macOS/Linux"
            )
        );
    }
}

#[cfg(test)]
mod read_allow_tests {
    use super::*;

    #[derive(Default)]
    struct FakeLister {
        entries: BTreeMap<PathBuf, Result<Vec<ListedReadChild>, String>>,
    }

    impl FakeLister {
        fn directory(mut self, parent: &str, children: &[(&str, bool)]) -> Self {
            let parent = PathBuf::from(parent);
            self.entries.insert(
                parent.clone(),
                Ok(children
                    .iter()
                    .map(|(name, is_dir)| ListedReadChild {
                        path: parent.join(name),
                        is_dir: *is_dir,
                    })
                    .collect()),
            );
            self
        }

        fn failure(mut self, parent: &str, message: &str) -> Self {
            self.entries
                .insert(PathBuf::from(parent), Err(message.to_string()));
            self
        }
    }

    impl ReadDirectoryLister for FakeLister {
        fn children(&mut self, parent: &Path) -> Result<Vec<ListedReadChild>, String> {
            self.entries
                .remove(parent)
                .unwrap_or_else(|| Err(format!("unexpected enumeration of {}", parent.display())))
        }
    }

    fn grant(path: &str, force_children: bool, mandatory: bool) -> IntendedReadGrant {
        IntendedReadGrant {
            path: PathBuf::from(path),
            force_children,
            mandatory,
        }
    }

    #[test]
    fn read_grants_split_home_across_all_deny_chains() {
        let mut lister = FakeLister::default()
            .directory(
                "/home/alice",
                &[
                    (".ssh", true),
                    (".config", true),
                    ("work", true),
                    ("notes", false),
                ],
            )
            .directory(
                "/home/alice/.config",
                &[("gcloud", true), ("cortexkit", true), ("editor", true)],
            )
            .directory("/home/alice/work", &[("private", true), ("src", true)]);
        let denies = [
            "/home/alice/.ssh",
            "/home/alice/.config/gcloud",
            "/home/alice/.config/cortexkit",
            "/home/alice/work/private",
        ]
        .map(PathBuf::from);

        let emitted = split_read_grants(&[grant("/home/alice", true, false)], &denies, &mut lister)
            .expect("split HOME grants");

        assert_eq!(
            emitted,
            [
                "/home/alice/.config/editor",
                "/home/alice/notes",
                "/home/alice/work/src",
            ]
            .map(PathBuf::from)
        );
    }

    #[test]
    fn secure_enumeration_omits_home_child_symlinks() {
        let mut lister = FakeLister::default()
            .directory("/home/alice", &[("ordinary", true), ("plain-file", false)]);
        let emitted = split_read_grants(
            &[grant("/home/alice", true, false)],
            &[PathBuf::from("/home/alice/.ssh")],
            &mut lister,
        )
        .expect("split HOME grants");

        assert_eq!(
            emitted,
            ["/home/alice/ordinary", "/home/alice/plain-file"].map(PathBuf::from)
        );
        assert!(!emitted.iter().any(|path| path.ends_with("secret-link")));
    }

    #[test]
    fn enumeration_race_refuses_instead_of_weakening_the_floor() {
        let mut lister = FakeLister::default().failure("/home/alice", "entry disappeared");
        let error = split_read_grants(
            &[grant("/home/alice", true, false)],
            &[PathBuf::from("/home/alice/.ssh")],
            &mut lister,
        )
        .expect_err("racing enumeration must fail closed");

        assert!(error.contains("cannot split read root /home/alice"));
        assert!(error.contains("entry disappeared"));
    }

    #[test]
    fn mandatory_floor_rejects_equal_containing_and_nested_writable_roots() {
        let floor = vec![PathBuf::from("/home/alice/.ssh")];
        for writable in [
            Path::new("/home/alice/.ssh"),
            Path::new("/home/alice"),
            Path::new("/home/alice/.ssh/cache"),
        ] {
            let error = validate_mandatory_floor_overlap([writable], &floor)
                .expect_err("mandatory floor overlap must refuse");
            assert!(error.contains("overlaps mandatory secret floor"));
        }
        validate_mandatory_floor_overlap([Path::new("/home/alice/project")], &floor)
            .expect("disjoint writable root");
    }

    #[test]
    fn ordinary_read_deny_under_writable_root_is_split_not_refused() {
        let mut lister =
            FakeLister::default().directory("/project", &[("private", true), ("src", true)]);
        let writable_root = PathBuf::from("/project");
        let emitted = split_read_grants(
            &[IntendedReadGrant {
                path: writable_root.clone(),
                force_children: false,
                mandatory: false,
            }],
            &[PathBuf::from("/project/private")],
            &mut lister,
        )
        .expect("ordinary deny should be expressible");

        assert_eq!(emitted, vec![PathBuf::from("/project/src")]);
        assert_eq!(writable_root, PathBuf::from("/project"));
    }

    #[test]
    fn static_var_grant_splits_when_home_is_beneath_it() {
        let mut lister = FakeLister::default()
            .directory("/var", &[("home", true), ("log", true)])
            .directory("/var/home", &[("alice", true)])
            .directory("/var/home/alice", &[(".ssh", true), ("work", true)]);
        let emitted = split_read_grants(
            &[grant("/var", true, true)],
            &[PathBuf::from("/var/home/alice/.ssh")],
            &mut lister,
        )
        .expect("split /var around HOME floor");

        assert_eq!(
            emitted,
            ["/var/home/alice/work", "/var/log"].map(PathBuf::from)
        );
    }

    #[test]
    fn run_sensitive_directories_are_removed_by_canonical_deny_chain() {
        let mut lister = FakeLister::default().directory(
            "/run",
            &[
                ("lock", true),
                ("user", true),
                ("credentials", true),
                ("secrets", true),
            ],
        );
        let emitted = split_read_grants(
            &[grant("/run", false, true)],
            &[
                PathBuf::from("/run/user"),
                PathBuf::from("/run/credentials"),
                PathBuf::from("/run/secrets"),
            ],
            &mut lister,
        )
        .expect("split /run");

        assert_eq!(emitted, vec![PathBuf::from("/run/lock")]);
        assert!(!emitted.iter().any(|path| {
            path == Path::new("/run/credentials") || path == Path::new("/run/secrets")
        }));
    }

    #[test]
    fn final_validation_rejects_every_overlap_direction() {
        let deny = vec![PathBuf::from("/home/alice/.ssh")];
        for grant in [
            PathBuf::from("/home/alice"),
            PathBuf::from("/home/alice/.ssh"),
            PathBuf::from("/home/alice/.ssh/key"),
        ] {
            assert!(validate_final_read_rules(&[grant], &deny).is_err());
        }
        validate_final_read_rules(&[PathBuf::from("/home/alice/work")], &deny)
            .expect("disjoint final grant");
    }

    #[test]
    fn grant_beneath_ordinary_deny_is_dropped_but_mandatory_grant_refuses() {
        let deny = vec![PathBuf::from("/restricted")];
        let mut lister = FakeLister::default();
        let emitted = split_read_grants(
            &[grant("/restricted/project", false, false)],
            &deny,
            &mut lister,
        )
        .expect("ordinary grant is optional");
        assert!(emitted.is_empty());

        let error = split_read_grants(
            &[grant("/restricted/system", false, true)],
            &deny,
            &mut lister,
        )
        .expect_err("mandatory grant under deny must refuse");
        assert!(error.contains("mandatory read root"));
    }

    #[cfg(unix)]
    #[test]
    fn linked_worktree_resolves_common_git_dir_and_shared_hooks() {
        // Neutralize ambient co_author hooksPath env injection (highest git
        // config scope) so the worktree's own hooks resolution decides.
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let fixture = tempfile::tempdir().expect("fixture");
        let main = fixture.path().join("main");
        let worktree = fixture.path().join("linked");
        std::fs::create_dir(&main).expect("main repository");
        assert!(Command::new("git")
            .args(["init", "-q"])
            .current_dir(&main)
            .status()
            .expect("git init")
            .success());
        std::fs::write(main.join("tracked"), b"tracked").expect("tracked file");
        assert!(Command::new("git")
            .args(["add", "tracked"])
            .current_dir(&main)
            .status()
            .expect("git add")
            .success());
        assert!(Command::new("git")
            .args([
                "-c",
                "user.name=AFT Test",
                "-c",
                "user.email=aft@example.invalid",
                "commit",
                "-qm",
                "initial",
            ])
            .current_dir(&main)
            .status()
            .expect("git commit")
            .success());
        assert!(Command::new("git")
            .args(["worktree", "add", "-q"])
            .arg(&worktree)
            .arg("HEAD")
            .current_dir(&main)
            .status()
            .expect("git worktree add")
            .success());

        let policy = resolve_git_policy(&worktree).expect("resolve linked worktree policy");
        let common = main.join(".git").canonicalize().expect("common git dir");
        assert_eq!(policy.hooks, vec![common.join("hooks")]);
        #[cfg(target_os = "linux")]
        assert!(policy.read_roots.contains(&common));
    }

    #[cfg(unix)]
    #[test]
    fn configured_hooks_path_is_resolved_to_its_effective_location() {
        // Neutralize ambient co_author hooksPath env injection (highest git
        // config scope) so the temp repo's local core.hooksPath decides.
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let fixture = tempfile::tempdir().expect("fixture");
        let project = fixture.path().join("project");
        std::fs::create_dir(&project).expect("project");
        assert!(Command::new("git")
            .args(["init", "-q"])
            .current_dir(&project)
            .status()
            .expect("git init")
            .success());
        assert!(Command::new("git")
            .args(["config", "core.hooksPath", "custom-hooks"])
            .current_dir(&project)
            .status()
            .expect("git config")
            .success());

        let policy = resolve_git_policy(&project).expect("resolve configured hooks path");
        assert_eq!(
            policy.hooks,
            vec![project
                .canonicalize()
                .expect("canonical project")
                .join("custom-hooks")]
        );
    }
}
