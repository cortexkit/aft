//! Policy and process wiring for agent-provided shell commands.
//!
//! Every agent bash process reaches [`resolve_sandbox_spawn`] and carries the
//! resulting [`SpawnPlan`] into one of the two process-creation primitives:
//! detached pipes or PTY. Foreground orchestration uses the detached registry
//! too, so it does not create a third process-creation path.
//!
//! Other implementation processes remain outside this seam: LSP servers and Windows LSP cleanup in
//! `lsp::client` and `lsp::child_registry`; git probes in `search_index`,
//! `readonly_artifacts`, `commands::configure`, and `commands::conflicts`; login
//! shell PATH discovery in `effective_path`; and Windows process liveness or
//! termination helpers in `artifact_owner`, `fs_lock`, and
//! `bash_background::process`. Image and PDF handling in `commands::read` is
//! in-process and creates no child. Keeping this inventory here makes the
//! agent-command boundary explicit without accidentally applying agent policy to
//! AFT's internal tooling.
//! Formatters, their version probes, and checkers do execute project code, so
//! [`ToolLaunchPolicy`] carries the session shell's launch policy to those tools.

use std::cell::RefCell;
#[cfg(any(test, target_os = "linux"))]
use std::collections::BTreeSet;
use std::collections::{BTreeMap, HashMap};
#[cfg(unix)]
use std::collections::{HashSet, VecDeque};
#[cfg(target_os = "linux")]
use std::ffi::{CStr, CString};
use std::ffi::{OsStr, OsString};
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
use std::path::{Path, PathBuf};
use std::process::Command;
#[cfg(unix)]
use std::sync::Arc;
#[cfg(unix)]
use std::sync::Condvar;
use std::sync::{Mutex, OnceLock};
#[cfg(unix)]
use std::time::{Duration, Instant};

use portable_pty::CommandBuilder;

use crate::config::SandboxMode;
use crate::context::AppContext;
#[cfg(unix)]
use crate::sandbox_profile::resolved_env_deny_paths;
use crate::sandbox_profile::SandboxProfile;

#[cfg(unix)]
mod task_temp;

pub const SANDBOX_UNAVAILABLE_EXIT_CODE: i32 = 78;

/// Server-authenticated trust classification for a route bind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum PrincipalTrust {
    FirstParty,
    Untrusted,
}

/// Principal data supplied by the server-side transport, never by a bash body.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
    /// The grant was used up by a refused consume attempt, so its payload
    /// bundle was never handed to a task.
    refused: bool,
    /// Set once the grant can no longer hand its payload bundle to a task
    /// (it expired or was refused) and the bundle has been scheduled for
    /// deletion. The metadata stays so a replay still gets its exact refusal.
    payload_reclaimed: bool,
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

    /// Mark every grant whose payload can no longer be claimed (expired, or
    /// refused on a consume attempt) and return those bundles' locations so
    /// the caller can delete them after releasing the store lock. A grant
    /// consumed successfully keeps its bundle: the running task owns it.
    fn take_unclaimable_payloads(&mut self, now: Instant) -> Vec<(PathBuf, String)> {
        let mut payloads = Vec::new();
        for grant in self.grants.values_mut() {
            let expired_unused = !grant.consumed && now >= grant.expires_at;
            if grant.payload_reclaimed || !(grant.refused || expired_unused) {
                continue;
            }
            grant.payload_reclaimed = true;
            payloads.push((grant.session_dir.clone(), grant.task_id.clone()));
        }
        payloads
    }
}

/// Delete escalation payload bundles that no task can claim any more.
#[cfg(unix)]
fn delete_unclaimable_payloads(payloads: Vec<(PathBuf, String)>) {
    for (session_dir, task_id) in payloads {
        if let Ok(task) = crate::bash_background::persistence::resolve_uninitialized_task_layout(
            &session_dir,
            &task_id,
        ) {
            let _ = crate::bash_background::persistence::delete_resolved_task(&task);
        }
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
    ExternalTool,
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
    /// Windows relaxed commands run without OS protection and must report it.
    RelaxedUnenforced,
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

#[cfg(unix)]
#[derive(serde::Serialize, serde::Deserialize)]
enum SavedSpawnPolicy {
    Unsandboxed,
    Host {
        shell_path: PathBuf,
    },
    Launcher {
        profile: SandboxProfile,
        launcher_path: PathBuf,
    },
}

/// Private recovery data, never serialized into agent-readable task metadata.
/// The environment is exactly the one already in the verified private payload;
/// memory-only shim tickets and daemon credentials are not added to it.
#[cfg(unix)]
#[derive(serde::Serialize, serde::Deserialize)]
struct SavedLocalLaunch {
    policy: SavedSpawnPolicy,
    root: PathBuf,
    cwd: PathBuf,
    principal: AuthenticatedPrincipal,
    shell_path: PathBuf,
    environment: Vec<(OsString, OsString)>,
    command: Vec<u8>,
    payload_digest: [u8; 32],
    linux_scope: bool,
    // Snapshots without a stored ceiling represent originally uncapped launches.
    #[serde(default)]
    memory_limit_mib: Option<u64>,
}

#[cfg(unix)]
pub(crate) const REMOTE_LOCAL_LAUNCH: &str = "remote-local-launch";

#[cfg(unix)]
pub(crate) fn save_local_launch(
    plan: &SpawnPlan,
    root: &Path,
    cwd: &Path,
    principal: &AuthenticatedPrincipal,
    shell: &Path,
    linux_scope: bool,
    memory_limit_mib: Option<u64>,
) -> Result<String, String> {
    let prepared = plan
        .prepared_task()
        .ok_or("local fallback payload absent")?;
    let policy = match plan.policy() {
        SpawnPlan::Unsandboxed => SavedSpawnPolicy::Unsandboxed,
        SpawnPlan::RelaxedUnenforced => {
            return Err("an unenforced Windows plan cannot be saved on Unix".into());
        }
        SpawnPlan::Host { shell_path, .. } => SavedSpawnPolicy::Host {
            shell_path: shell_path.clone(),
        },
        SpawnPlan::Launcher {
            profile,
            launcher_path,
        } => SavedSpawnPolicy::Launcher {
            profile: profile.clone(),
            launcher_path: launcher_path.clone(),
        },
        _ => return Err("refused local launch cannot be saved".into()),
    };
    let snapshot = SavedLocalLaunch {
        policy,
        root: root.into(),
        cwd: cwd.into(),
        principal: principal.clone(),
        shell_path: plan.host_shell_path().unwrap_or(shell).into(),
        environment: prepared
            .environment()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        command: prepared.0.command_bytes.as_ref().clone(),
        payload_digest: *prepared.0.digest.as_bytes(),
        linux_scope,
        memory_limit_mib,
    };
    let bytes = serde_json::to_vec(&snapshot).map_err(|e| e.to_string())?;
    let mut file = crate::bash_background::persistence::create_control_file(
        &prepared.0.dirs,
        REMOTE_LOCAL_LAUNCH,
        &bytes,
    )
    .map_err(|e| e.to_string())?;
    use std::io::Write;
    file.flush().map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())?;
    let mut hash = blake3::Hasher::new();
    hash.update(prepared.0.paths.task_id.as_bytes());
    hash.update(&bytes);
    Ok(hash.finalize().to_hex().to_string())
}

/// Reopen the frozen launch policy and verify the original payload. A missing
/// or changed snapshot refuses recovery; current configuration is never used.
#[cfg(unix)]
pub(crate) fn restore_local_launch(
    task: &crate::bash_background::persistence::ResolvedTask,
    expected: &str,
) -> Result<
    (
        SpawnPlan,
        PathBuf,
        HashMap<String, String>,
        bool,
        Option<u64>,
    ),
    String,
> {
    let mut file =
        crate::bash_background::persistence::open_control_file(task, REMOTE_LOCAL_LAUNCH)
            .map_err(|e| e.to_string())?;
    let bytes = read_held_payload(&mut file)?;
    let mut hash = blake3::Hasher::new();
    hash.update(task.paths.task_id.as_bytes());
    hash.update(&bytes);
    if hash.finalize().to_hex().as_str() != expected {
        return Err("local fallback snapshot digest mismatch".into());
    }
    let snapshot: SavedLocalLaunch = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    let environment: ChildEnvironment = snapshot.environment.into_iter().collect();
    let plan = match snapshot.policy {
        SavedSpawnPolicy::Unsandboxed => SpawnPlan::Unsandboxed,
        SavedSpawnPolicy::Host { shell_path } => SpawnPlan::Host {
            shell_path,
            environment: environment.clone(),
        },
        SavedSpawnPolicy::Launcher {
            profile,
            launcher_path,
        } => SpawnPlan::Launcher {
            profile,
            launcher_path,
        },
    };
    let prepared = verify_payload(
        crate::bash_background::persistence::ResolvedTask {
            paths: task.paths.clone(),
            dirs: task.dirs.clone(),
        },
        &snapshot.command,
        &snapshot.root,
        &snapshot.cwd,
        &snapshot.principal,
        &snapshot.shell_path,
        &environment,
        Some(blake3::Hash::from_bytes(snapshot.payload_digest)),
        false,
    )?;
    let env = environment
        .into_iter()
        .map(|(k, v)| {
            Ok((
                k.into_string()
                    .map_err(|_| "non-Unicode fallback environment key")?,
                v.into_string()
                    .map_err(|_| "non-Unicode fallback environment value")?,
            ))
        })
        .collect::<Result<HashMap<_, _>, String>>()?;
    Ok((
        plan.with_prepared_task(prepared),
        snapshot.shell_path,
        env,
        snapshot.linux_scope,
        snapshot.memory_limit_mib,
    ))
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
    pub(crate) fn changed_files_read_profile(&self) -> Option<&SandboxProfile> {
        match self.policy() {
            Self::Launcher { profile, .. } => Some(profile),
            _ => None,
        }
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

    /// Strict native backends fail closed if containment cannot be installed.
    /// Merely using the launcher is insufficient: off installs no policy and
    /// relaxed is not a boundary for arbitrary outside-project operations.
    pub(crate) fn enforces_shell_containment(&self) -> bool {
        cfg!(any(target_os = "linux", target_os = "macos"))
            && matches!(self.policy(), Self::Launcher { profile, .. } if profile.mode == SandboxMode::Strict)
    }

    /// Why a jailed worker's shell launched under this plan must not receive
    /// the host-local GitHub read broker, or `None` when it may. The broker's
    /// isolation needs an OS policy that keeps each worker out of the shared
    /// task-temp root: the worker must not read another task's capability
    /// file or replace another task's broker socket with its own listener.
    /// Strict enforces that on macOS and Linux. Relaxed enforces it only on
    /// macOS (Seatbelt applies its denies); relaxed Linux grants all of `/`
    /// and enforces no deny. Off is the operator's explicit opt-out of the
    /// jail. This is the only profile rule for the broker, so the profiles
    /// cannot drift apart.
    #[cfg(unix)]
    pub(crate) fn worker_read_broker_unavailable(&self) -> Option<&'static str> {
        if !cfg!(any(target_os = "linux", target_os = "macos")) {
            return Some("the native worker jail runs only on macOS and Linux");
        }
        match self.policy() {
            Self::Launcher { profile, .. } => match profile.mode {
                SandboxMode::Strict => None,
                SandboxMode::Relaxed if cfg!(target_os = "macos") => None,
                SandboxMode::Relaxed => Some(
                    "sandbox profile relaxed on Linux does not protect the shared task-temp root, so another worker could read this command's capability or replace its broker socket",
                ),
                SandboxMode::Off => Some(
                    "sandbox profile off runs the worker shell without the worker jail",
                ),
            },
            _ => Some("the command does not run under the native worker launcher"),
        }
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
            let _ = cleanup_task_temp_dir(temp_dir);
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

#[cfg(all(unix, any(test, feature = "test-sandbox-launcher")))]
static TEST_LAUNCHERS: OnceLock<Mutex<HashMap<PathBuf, PathBuf>>> = OnceLock::new();

thread_local! {
    static CURRENT_PRINCIPAL: RefCell<Option<AuthenticatedPrincipal>> = const { RefCell::new(None) };
    static WORKER_JAIL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
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

struct WorkerJailScope(bool);

impl Drop for WorkerJailScope {
    fn drop(&mut self) {
        WORKER_JAIL.with(|slot| slot.set(self.0));
    }
}

/// Select the worker shell profile after the route's Alfonso worker stamp
/// has been authenticated; the flag is scoped to this request's dispatch.
pub(crate) fn with_worker_jail<R>(jailed: bool, run: impl FnOnce() -> R) -> R {
    let previous = WORKER_JAIL.with(|slot| slot.replace(jailed));
    let _scope = WorkerJailScope(previous);
    run()
}

/// Whether the shell dispatch on this thread belongs to a jailed worker.
pub(crate) fn worker_jail_active() -> bool {
    WORKER_JAIL.with(std::cell::Cell::get)
}

pub(crate) fn effective_sandbox_mode(
    ctx: &AppContext,
    principal: &AuthenticatedPrincipal,
) -> SandboxMode {
    if principal_requires_strict(principal) {
        return SandboxMode::Strict;
    }
    let profiles = &ctx.config().sandbox.profile;
    if worker_jail_active() {
        profiles.worker
    } else {
        profiles.head
    }
}

pub(crate) fn principal_requires_strict(principal: &AuthenticatedPrincipal) -> bool {
    // An MCP harness claim can only tighten confinement, including if a facade is
    // mistakenly stamped first-party. It never grants trust or relaxes a profile.
    !principal_is_first_party(principal)
        || matches!(principal, AuthenticatedPrincipal::RouteBind { harness, principal_id, .. }
        if harness.starts_with("mcp:") || principal_id.as_deref().is_none_or(|id| id == "unverified"))
}

/// Resolve the role and authenticated principal before choosing the launcher.
pub(crate) fn sandbox_required(ctx: &AppContext) -> bool {
    effective_sandbox_mode(ctx, &current_authenticated_principal()) != SandboxMode::Off
        || (cfg!(unix) && worker_jail_active())
}

fn worker_jail_refusal(reason: &str) -> SpawnPlan {
    SpawnPlan::Refused {
        code: "sandbox_unavailable",
        message: format!(
            "worker shells must run in the native sandbox, which cannot be enforced here: {reason}"
        ),
        mismatch_class: None,
    }
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
    let unclaimable = store.take_unclaimable_payloads(now);
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
        refused: false,
        payload_reclaimed: false,
        session_dir: prepared.0.paths.session_dir.clone(),
        task_id: prepared.0.paths.task_id.clone(),
    };
    store.grants.insert(grant_id.clone(), grant);
    drop(store);
    delete_unclaimable_payloads(unclaimable);
    Ok(grant_id)
}

/// Validate one consume attempt and mark the grant used. A refusal that uses
/// up the grant also marks it refused, so its payload bundle is reclaimed.
#[cfg(unix)]
fn check_escalation_grant(
    store: &mut EscalationGrantStore,
    principal: &AuthenticatedPrincipal,
    attempt: &HostEscalationAttempt,
    now: Instant,
) -> Result<(blake3::Hash, PathBuf, String), EscalationRefusal> {
    let Some(grant) = store.grants.get_mut(&attempt.grant_id) else {
        return Err(EscalationRefusal::DigestMismatch);
    };
    if grant.consumed {
        return Err(EscalationRefusal::Consumed);
    }
    let refusal = if now >= grant.expires_at {
        Some(EscalationRefusal::Expired)
    } else if grant.principal != *principal {
        Some(EscalationRefusal::WrongPrincipal)
    } else if grant.root != attempt.root {
        Some(EscalationRefusal::DigestMismatch)
    } else {
        None
    };
    grant.consumed = true;
    if let Some(refusal) = refusal {
        grant.refused = true;
        return Err(refusal);
    }
    Ok((
        grant.digest,
        grant.session_dir.clone(),
        grant.task_id.clone(),
    ))
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
        let checked = check_escalation_grant(&mut store, principal, attempt, now);
        let unclaimable = store.take_unclaimable_payloads(now);
        drop(store);
        delete_unclaimable_payloads(unclaimable);
        checked?
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

/// Whether this Unix request must use the native launcher. Off-profile workers
/// also use it to preserve their private temp directory and stripped environment,
/// but install no OS policy.
///
/// Process spawning and in-process bash rewriting share this check so neither
/// confinement nor the worker environment can be bypassed by a rewrite.
pub(crate) fn native_sandbox_enforced(
    ctx: &AppContext,
    principal: &AuthenticatedPrincipal,
) -> bool {
    cfg!(unix)
        && (effective_sandbox_mode(ctx, principal) != SandboxMode::Off || worker_jail_active())
}

pub(crate) fn unsupported_platform_sandbox_refusal(ctx: &AppContext) -> Option<SpawnPlan> {
    unsupported_profile_sandbox_refusal(effective_sandbox_mode(
        ctx,
        &current_authenticated_principal(),
    ))
}

fn unsupported_profile_sandbox_refusal(mode: SandboxMode) -> Option<SpawnPlan> {
    platform_sandbox_plan(mode, RequestedSandboxTier::Native, cfg!(unix))
        .filter(|plan| plan.refusal_code().is_some())
}

fn platform_sandbox_plan(
    mode: SandboxMode,
    requested_tier: RequestedSandboxTier,
    native_backend: bool,
) -> Option<SpawnPlan> {
    if native_backend {
        return None;
    }
    match mode {
        SandboxMode::Off => return Some(SpawnPlan::Unsandboxed),
        SandboxMode::Relaxed => {
            return Some(if requested_tier == RequestedSandboxTier::Native {
                SpawnPlan::RelaxedUnenforced
            } else {
                worker_jail_refusal("the command asked to leave the configured relaxed profile")
            });
        }
        SandboxMode::Strict => {}
    }
    Some(SpawnPlan::Refused {
        code: "sandbox_unavailable",
        message: format!("sandbox profile {mode} is not enforced on Windows; select sandbox.profile off or run on macOS/Linux"),
        mismatch_class: None,
    })
}

/// Resolve policy for an agent-command process.
///
/// `task_bundle_dir` must be the already-created directory that owns the task's
/// capture files. The native builder grants a fresh private temp directory
/// beneath it; capture files themselves use pre-opened descriptors.
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

    // Strict must fail closed without a backend. Relaxed explicitly reports
    // unenforced denies instead, so Windows workers can run their default profile.
    let mode = effective_sandbox_mode(ctx, principal);
    if let Some(plan) = platform_sandbox_plan(mode, requested_tier, cfg!(unix)) {
        return plan;
    }
    if requested_tier == RequestedSandboxTier::Host && principal_requires_strict(principal) {
        return SpawnPlan::Refused {
            code: "sandbox_escalation_denied",
            message: "sandbox host escalation is unavailable to untrusted principals".to_string(),
            mismatch_class: Some("wrong_principal"),
        };
    }

    if mode != SandboxMode::Off && (worker_jail_active() || !principal_is_first_party(principal)) {
        // A jailed worker never runs unconfined: neither an unsandboxed tier
        // nor a host escalation, nor a principal the native launcher refuses.
        if requested_tier != RequestedSandboxTier::Native {
            return worker_jail_refusal("the command asked to leave the sandbox");
        }
        if !native_sandbox_enforced(ctx, principal) {
            return worker_jail_refusal("the route's principal cannot use the native launcher");
        }
    }

    // An off-profile worker still uses the launcher for its private temp and
    // credential-stripped environment; the launcher installs no OS policy.
    if mode == SandboxMode::Off && (!cfg!(unix) || !worker_jail_active()) {
        return SpawnPlan::Unsandboxed;
    }
    if requested_tier == RequestedSandboxTier::Disabled && mode != SandboxMode::Off {
        return worker_jail_refusal("the command asked to disable the configured sandbox profile");
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
        let launcher_path = match native_launcher_path(ctx) {
            Ok(path) => path,
            Err(error) => {
                let _ = cleanup_task_temp_dir(&profile.temp_dir);
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
        "sandbox setup for {} failed: {cause}; select sandbox.profile off to disable native sandboxing",
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

    if effective_sandbox_mode(ctx, principal) == SandboxMode::Off {
        let temp = create_task_temp_dir(&task_io_dir)?;
        let result = SandboxProfile::build(
            project_roots,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            temp.clone(),
        );
        return match result {
            Ok(mut profile) => {
                profile.mode = SandboxMode::Off;
                if worker_jail_active() {
                    profile.worker_gh_fence =
                        Some(crate::sandbox_profile::WorkerGhFence::Unenforced);
                }
                Ok(profile)
            }
            Err(error) => {
                let _ = cleanup_task_temp_dir(&temp);
                Err(error.to_string())
            }
        };
    }

    let git_policies = project_roots
        .iter()
        .map(|root| resolve_git_policy(root))
        .collect::<Result<Vec<_>, _>>()?;
    let temp_dir = create_task_temp_dir(&task_io_dir)?;
    let result = (|| {
        if effective_sandbox_mode(ctx, principal) == SandboxMode::Relaxed {
            return build_relaxed_profile(
                ctx,
                &home,
                &project_roots,
                &git_policies,
                &session_store,
                &task_io_dir,
                &temp_dir,
            );
        }
        let mut policy = ctx.access_policy("").map_err(|response| {
            response.data["message"]
                .as_str()
                .unwrap_or("access policy could not be resolved")
                .to_string()
        })?;
        policy.roots = project_roots.clone();
        policy
            .private_stores
            .read_allow
            .extend(project_roots.iter().cloned());
        policy.private_stores.read_allow.sort_unstable();
        policy.private_stores.read_allow.dedup();
        policy.private_stores.write_allow = project_roots.clone();
        policy.add_session_store_floor(session_store.clone());
        policy.private_stores.session_task_read_dir = Some(session_store.clone());
        let mut write_deny_nested = Vec::new();
        let mut shell_write_deny = Vec::new();
        let mut shell_read_deny = Vec::new();
        let jailed = worker_jail_active();
        // Ordinary sessions retain writable project Git configuration; jailed
        // workers and delegated write roots must not change execution controls.
        let mut control_roots = if jailed {
            project_roots.clone()
        } else {
            Vec::new()
        };
        control_roots.extend(policy.shell_grants(crate::access_policy::AccessOperation::Write));
        control_roots.sort_unstable();
        control_roots.dedup();
        for root in &control_roots {
            let git = resolve_git_policy(root)?;
            write_deny_nested.extend(checkout_control_denies(root, &git)?);
        }
        // Only a worker jail denies the gh-shim canary; that deny is how the
        // shim tells AFT's worker jail from any other sandbox. A canary that
        // cannot be created refuses the worker launch. Head shells get an
        // explicit read grant so a canary left by an earlier worker, inside
        // otherwise private storage, never makes a head look jailed.
        let canary_dirs = crate::worker_jail_canary::prepare_homes(&home, jailed)?;
        if !jailed {
            policy
                .private_stores
                .read_allow
                .extend(canary_dirs.iter().cloned());
        }
        #[cfg(target_os = "macos")]
        if jailed {
            install_worker_mktemp(&temp_dir)?;
        }
        for (root, git_policy) in project_roots.iter().zip(&git_policies) {
            #[cfg(target_os = "linux")]
            write_deny_nested.push(root.join(".git"));
            if !jailed {
                write_deny_nested.push(root.join(".cortexkit"));
            }
            #[cfg(target_os = "macos")]
            shell_write_deny.extend(git_policy.hooks.iter().cloned());
            if jailed {
                // Git's configured policy hooks must execute on commits. Keep
                // them immutable even when their directory is in private data.
                policy.private_stores.read_allow.extend(
                    git_policy
                        .hooks
                        .iter()
                        .filter(|path| profile_candidate_is_dir(path))
                        .cloned(),
                );
                policy
                    .private_stores
                    .read_allow
                    .extend(git_policy.read_roots.iter().cloned());
                policy
                    .private_stores
                    .read_metadata_allow
                    .extend(git_policy.metadata_allow.iter().cloned());
                for (key, path) in &git_policy.configured_paths {
                    if (key == "core.excludesFile" && profile_candidate_is_file(path))
                        || (key != "core.excludesFile" && profile_candidate_is_dir(path))
                    {
                        policy.private_stores.read_allow.push(path.clone());
                        if key == "core.excludesFile"
                            && git_policy
                                .linked
                                .as_ref()
                                .is_some_and(|(git_dir, _)| path.starts_with(git_dir))
                        {
                            // core.excludesFile can be inside the linked worktree's Git
                            // directory. Landlock cannot subtract a file from that
                            // writable directory; report the nested exclusion
                            // instead of refusing all Git commands.
                            write_deny_nested.push(path.clone());
                        } else {
                            shell_write_deny.push(path.clone());
                        }
                    }
                }
            } else {
                // Non-worker native shells still cannot read repository hooks.
                shell_read_deny.extend(git_policy.hooks.iter().cloned());
            }
        }
        let storage = ctx.storage_dir();
        let mut managed_paths = Vec::new();
        if ctx.config().github.shim {
            managed_paths.push(storage.join(crate::agent_child_env::SHIMS_DIR_NAME));
            managed_paths.push(crate::agent_child_env::shim_binary(&ctx.config())?);
        }
        if ctx.config().git.co_author != "off" {
            managed_paths.push(crate::agent_child_env::managed_git_hooks_dir(&storage));
            if ctx.config().git.co_author == "auto" {
                managed_paths.push(crate::agent_child_env::shim_binary(&ctx.config())?);
            }
        }
        // Governance injection materializes these before resolving a spawn.
        // Do not grant a missing shim directory or a broad binary parent.
        managed_paths.retain(|path| profile_candidate_exists(path));
        shell_write_deny.extend(managed_paths);

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
        #[cfg(test)]
        cache_roots.extend(candidate_snapshot::cache_candidates());
        cache_roots.retain(|path| profile_candidate_is_dir(path));
        // A jailed worker's shell keeps writes to its worktree, plus what normal
        // builds and commits need, all taken from operator configuration.
        let jail = worker_jail_active()
            .then(|| worker_jail_grants(&home, &project_roots, &git_policies, &policy));
        if let Some(jail) = &jail {
            policy
                .private_stores
                .read_allow
                .extend(jail.read_only.iter().cloned());
            policy
                .private_stores
                .read_allow
                .extend(jail.files.iter().cloned());
            policy
                .private_stores
                .write_allow
                .extend(jail.files.iter().cloned());
            shell_write_deny.extend(jail.read_only.iter().cloned());
            cache_roots.extend(jail.caches.iter().cloned());
            cache_roots.sort_unstable();
            cache_roots.dedup();
        }

        let mut socket_deny = vec![PathBuf::from("/var/run/docker.sock")];
        if let Some(agent_socket) =
            std::env::var_os("SSH_AUTH_SOCK").filter(|value| !value.is_empty())
        {
            socket_deny.push(PathBuf::from(agent_socket));
        }

        let mut profile = policy
            .compile_shell(temp_dir.clone(), cache_roots, socket_deny)
            .map_err(|error| error.to_string())?;
        profile.write_deny.extend(shell_write_deny);
        profile.write_deny_nested = write_deny_nested;
        profile.read_deny.extend(shell_read_deny);
        add_credential_floor(&mut profile, &home, &project_roots)?;
        if jailed {
            profile.read_deny.extend(canary_dirs.iter().cloned());
            profile.write_deny.extend(canary_dirs.iter().cloned());
            profile.worker_gh_fence = Some(crate::sandbox_profile::WorkerGhFence::Canary {
                paths: canary_dirs
                    .iter()
                    .map(|dir| dir.join(crate::worker_jail_canary::CANARY_FILE))
                    .collect(),
            });
        }
        if let Some(jail) = jail {
            // Read server destinations from the operator's environment, never
            // from a worker request or a file the worker can edit.
            profile.loopback_tcp_connect_deny =
                sccache_server_ports(std::env::var("SCCACHE_SERVER_PORT").ok().as_deref());
            if let Some(path) = std::env::var_os("SCCACHE_SERVER_UDS").filter(|v| !v.is_empty()) {
                let path = PathBuf::from(path);
                let path = if path.is_absolute() {
                    path
                } else {
                    std::env::current_dir()
                        .map_err(|error| format!("failed to resolve sccache socket: {error}"))?
                        .join(path)
                };
                profile.socket_deny.push(path);
            }
            profile.writable_roots.extend(jail.roots);
            profile.writable_roots.sort_unstable();
            profile.writable_roots.dedup();
            profile.writable_files = jail.files;
        }
        protect_worker_credential_services(&mut profile, &home)?;
        profile = profile
            .canonicalize_for_launch()
            .map_err(|error| error.to_string())?;
        refuse_store_overlap(&profile, &session_store, &task_io_dir)?;

        #[cfg(target_os = "linux")]
        let profile = {
            let git_read_roots = git_policies
                .iter()
                .flat_map(|policy| policy.read_roots.iter().cloned())
                .collect::<Vec<_>>();
            profile = build_linux_profile(profile, &home, &git_read_roots)?;
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
        verify_worker_gh_fence(&profile)?;

        Ok(profile)
    })();
    if result.is_err() {
        let _ = cleanup_task_temp_dir(&temp_dir);
    }
    result
}

/// Check the final worker-jail profile against its gh-shim canary before the
/// launch: the canary directory must be denied for reads and writes, and no
/// read or write grant may overlap it, including the canary file. A grant over an
/// ancestor would let a worker read the canary (Landlock grants are whole
/// subtrees) or rename a directory above it. Any such profile is refused.
#[cfg(unix)]
fn verify_worker_gh_fence(profile: &SandboxProfile) -> Result<(), String> {
    let Some(crate::sandbox_profile::WorkerGhFence::Canary { paths }) = &profile.worker_gh_fence
    else {
        return Ok(());
    };
    if paths.is_empty() {
        return Err("worker-jail canary paths are empty".into());
    }
    for path in paths {
        let dir = path
            .parent()
            .ok_or_else(|| format!("worker-jail canary has no directory: {}", path.display()))?;
        let covers = |grant: &Path| dir.starts_with(grant);
        if !profile.read_deny.iter().any(|deny| covers(deny)) {
            return Err("the worker jail does not deny reading its gh-shim canary".into());
        }
        if !profile.write_deny.iter().any(|deny| covers(deny)) {
            return Err("the worker jail does not deny writing its gh-shim canary".into());
        }
        let read_grants = profile
            .read_allow
            .iter()
            .chain(&profile.data_policy.read_allow)
            .chain(&profile.data_policy.session_task_read_dir);
        let write_grants = profile
            .write_allow_roots()
            .into_iter()
            .map(Path::to_path_buf)
            .chain(profile.writable_files.iter().cloned())
            .chain(profile.data_policy.write_allow.iter().cloned())
            .collect::<Vec<_>>();
        if let Some(grant) = read_grants
            .map(PathBuf::as_path)
            .chain(write_grants.iter().map(PathBuf::as_path))
            .find(|grant| dir.starts_with(grant) || grant.starts_with(dir))
        {
            return Err(format!(
            "worker jail grant {} covers its gh-shim canary {}; refusing to start an unfenced worker shell",
            grant.display(),
            dir.display()
        ));
        }
    }
    Ok(())
}

/// Build relaxed's protected-path list without predicting the caches, sockets
/// or services a build will need. Each backend reports which denies it enforces.
#[cfg(unix)]
fn build_relaxed_profile(
    ctx: &AppContext,
    home: &Path,
    roots: &[PathBuf],
    git_policies: &[GitPolicy],
    session_store: &Path,
    task_io: &Path,
    temp: &Path,
) -> Result<SandboxProfile, String> {
    let mut policy = ctx
        .access_policy("")
        .map_err(|error| error.data.to_string())?;
    let mut read_deny = policy.absolute_floor.clone();
    let mut write_deny = read_deny.clone();
    write_deny.extend(
        [
            ".zshrc",
            ".zprofile",
            ".bashrc",
            ".profile",
            ".gitconfig",
            ".config/git",
            "Library/LaunchAgents",
        ]
        .map(|path| home.join(path)),
    );
    let mut stores = policy.private_stores.clone();
    stores.deny.push(session_store.to_path_buf());
    stores.session_task_read_dir = Some(session_store.to_path_buf());
    stores.deny.push(home.join(".config/cortexkit"));
    stores.deny.extend([
        home.join(".local/share/cortexkit"),
        home.join(".local/state/cortexkit"),
        ctx.storage_dir(),
    ]);
    for key in ["XDG_DATA_HOME", "XDG_STATE_HOME", "XDG_CONFIG_HOME"] {
        if let Some(base) = absolute_env_path(key) {
            stores
                .deny
                .extend(resolved_env_deny_paths(key, base.join("cortexkit")));
        }
    }
    stores.read_allow = roots.to_vec();
    stores.write_allow = roots.to_vec();
    for data in [
        Some(home.join(".local/share")),
        absolute_env_path("XDG_DATA_HOME"),
    ]
    .into_iter()
    .flatten()
    {
        stores
            .read_allow
            .push(data.join("cortexkit/alfonso/worktrees"));
        stores.read_allow.push(data.join("cortexkit/bin"));
    }
    stores
        .read_allow
        .extend([temp.to_path_buf(), task_io.to_path_buf()]);
    stores
        .write_allow
        .extend([temp.to_path_buf(), task_io.to_path_buf()]);
    // Select grants against the complete credential and store floors before
    // protecting their control files or carving out sibling checkout denies.
    policy.absolute_floor = read_deny.clone();
    policy.private_stores = stores.clone();
    let write_grants = policy.shell_grants(crate::access_policy::AccessOperation::Write);
    let mut control_roots = roots.to_vec();
    control_roots.extend(write_grants.iter().cloned());
    control_roots.sort_unstable();
    control_roots.dedup();
    let control_git_policies = control_roots
        .iter()
        .map(|root| resolve_git_policy(root))
        .collect::<Result<Vec<_>, _>>()?;
    let mut writable = roots.to_vec();
    writable.extend(linked_git_grants(git_policies));
    let files = linked_git_file_grants(git_policies);
    stores.write_allow.extend(writable.iter().cloned());
    stores.write_allow.extend(files.iter().cloned());
    let mut checkout_denies = vec![home.join("Work")];
    for (root, git) in control_roots.iter().zip(&control_git_policies) {
        write_deny.extend(checkout_control_denies(root, git)?);
        stores.read_allow.extend(git.read_roots.iter().cloned());
        stores.read_allow.extend(git.hooks.iter().cloned());
        stores
            .read_allow
            .extend(git.configured_paths.iter().map(|(_, path)| path.clone()));
        stores
            .read_metadata_allow
            .extend(git.metadata_allow.iter().cloned());
        for key in ["core.hooksPath", "mason.originalHooksPath"] {
            if !git
                .configured_paths
                .iter()
                .any(|(existing, _)| existing == key)
            {
                let mut metadata = Vec::new();
                if let Some((_, path)) = configured_git_path(root, key, &mut metadata)? {
                    if profile_candidate_is_dir(&path) {
                        stores.read_allow.push(path);
                    }
                    stores.read_metadata_allow.extend(metadata);
                }
            }
        }
        let listing = git_policy_output(root, &["worktree", "list", "--porcelain", "-z"])
            .map_err(|error| format!("cannot enumerate sibling checkouts: {error}"))?;
        if listing.status.success() {
            for field in listing.stdout.split(|byte| *byte == 0) {
                if let Some(path) = field.strip_prefix(b"worktree ") {
                    use std::os::unix::ffi::OsStrExt;
                    let path = PathBuf::from(OsStr::from_bytes(path));
                    if !roots.contains(&path) {
                        checkout_denies.push(path);
                    }
                }
            }
        } else if !git.read_roots.is_empty() {
            return Err(format!(
                "cannot enumerate sibling checkouts: {}",
                String::from_utf8_lossy(&listing.stderr)
            ));
        }
    }
    let storage = ctx.storage_dir();
    for path in [
        storage.join(crate::agent_child_env::SHIMS_DIR_NAME),
        crate::agent_child_env::managed_git_hooks_dir(&storage),
    ] {
        if profile_candidate_exists(&path) {
            stores.read_allow.push(path.clone());
        }
        write_deny.push(path);
    }
    if let Ok(binary) = crate::agent_child_env::shim_binary(&ctx.config()) {
        if profile_candidate_exists(&binary) {
            stores.read_allow.push(binary.clone());
        }
        write_deny.push(binary);
    }
    stores
        .read_allow
        .retain(|path| profile_candidate_exists(path));
    writable.extend(write_grants.iter().cloned());
    stores.write_allow.extend(write_grants);
    stores
        .read_allow
        .extend(policy.shell_grants(crate::access_policy::AccessOperation::Read));
    // The worker jail's gh-shim fence, as in the strict builder. Relaxed Linux
    // grants all of `/` and enforces no deny, so there the fence is reported
    // as unenforced instead; macOS relaxed enforces explicit denies.
    let jailed = worker_jail_active();
    let mut worker_gh_fence = None;
    if jailed && cfg!(target_os = "linux") {
        worker_gh_fence = Some(crate::sandbox_profile::WorkerGhFence::Unenforced);
    } else if jailed {
        let dirs = crate::worker_jail_canary::prepare_homes(home, true)?;
        read_deny.extend(dirs.iter().cloned());
        write_deny.extend(dirs.iter().cloned());
        worker_gh_fence = Some(crate::sandbox_profile::WorkerGhFence::Canary {
            paths: dirs
                .iter()
                .map(|dir| dir.join(crate::worker_jail_canary::CANARY_FILE))
                .collect(),
        });
    } else {
        stores
            .read_allow
            .extend(crate::worker_jail_canary::prepare_homes(home, false)?);
    }
    let mut profile = SandboxProfile {
        v: crate::sandbox_profile::SANDBOX_PROFILE_VERSION,
        mode: SandboxMode::Relaxed,
        worker_jail: false,
        keychain_deny: Vec::new(),
        deny_prefixes: Vec::new(),
        data_policy: stores,
        writable_roots: writable,
        writable_files: files,
        write_deny,
        write_deny_nested: checkout_denies,
        read_allow: Vec::new(),
        read_allow_listed: Vec::new(),
        read_deny,
        socket_deny: Vec::new(),
        loopback_tcp_connect_deny: Vec::new(),
        cache_roots: Vec::new(),
        temp_dir: temp.to_path_buf(),
        worker_gh_fence,
    };
    add_credential_floor(&mut profile, home, roots)?;
    protect_worker_credential_services(&mut profile, home)?;
    profile
        .canonicalize_for_launch()
        .map_err(|error| error.to_string())
        .and_then(|profile| verify_worker_gh_fence(&profile).map(|()| profile))
}

/// Mark only authenticated, sandboxed workers for credential-service isolation.
#[cfg(unix)]
fn protect_worker_credential_services(
    profile: &mut SandboxProfile,
    _home: &Path,
) -> Result<(), String> {
    profile.worker_jail = worker_jail_active();
    if profile.worker_jail {
        protect_worker_private_temp(profile)?;
    }
    #[cfg(target_os = "macos")]
    if profile.worker_jail {
        // Security.framework uses the account's home even when the daemon was
        // started with an alternate HOME. Protect both locations.
        profile.keychain_deny = vec![
            _home.join("Library/Keychains"),
            macos_account_home()?.join("Library/Keychains"),
            PathBuf::from("/Library/Keychains"),
        ];
    }
    Ok(())
}

#[cfg(unix)]
fn protect_worker_private_temp(profile: &mut SandboxProfile) -> Result<(), String> {
    let root = task_temp::root();
    let own = &profile.temp_dir;
    let keep = |paths: &mut Vec<PathBuf>| -> Result<(), String> {
        let mut retained = Vec::with_capacity(paths.len());
        for path in paths.drain(..) {
            let resolved = crate::sandbox_profile::resolve_env_deny_paths(
                path.clone(),
                "worker scratch exception",
            )
            .map_err(|error| error.to_string())?;
            if !resolved
                .iter()
                .any(|path| path.starts_with(root) && !path.starts_with(own))
            {
                retained.push(path);
            }
        }
        *paths = retained;
        Ok(())
    };
    keep(&mut profile.data_policy.read_allow)?;
    keep(&mut profile.data_policy.write_allow)?;
    keep(&mut profile.data_policy.read_metadata_allow)?;
    profile.data_policy.deny.push(root.to_owned());
    profile.data_policy.read_allow.push(own.clone());
    profile.data_policy.write_allow.push(own.clone());
    #[cfg(target_os = "macos")]
    {
        let name = own
            .file_name()
            .ok_or("worker temp has no name")?
            .to_string_lossy();
        profile
            .data_policy
            .read_metadata_allow
            .push(own.with_file_name(format!("{name}.gh-read.sock")));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn macos_account_home() -> Result<PathBuf, String> {
    use std::os::unix::ffi::OsStrExt;
    let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
    let mut result = std::ptr::null_mut();
    // A caller-owned buffer keeps getpwuid_r safe across concurrent launches.
    let mut buffer = vec![0u8; 16384];
    // SAFETY: the passwd and byte buffer are writable for their declared sizes;
    // result is inspected only after getpwuid_r reports a populated entry.
    let status = unsafe {
        libc::getpwuid_r(
            libc::geteuid(),
            entry.as_mut_ptr(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut result,
        )
    };
    if status != 0 || result.is_null() {
        return Err(format!(
            "cannot resolve account home for keychain isolation (getpwuid_r: {status})"
        ));
    }
    // SAFETY: a successful getpwuid_r initialized entry and its pw_dir points
    // into the live buffer. Copy the string before the buffer is released.
    let path = unsafe {
        let entry = entry.assume_init();
        if entry.pw_dir.is_null() {
            return Err("account home is missing for keychain isolation".into());
        }
        PathBuf::from(OsStr::from_bytes(
            std::ffi::CStr::from_ptr(entry.pw_dir).to_bytes(),
        ))
    };
    if !path.is_absolute() {
        return Err("account home is not absolute for keychain isolation".into());
    }
    Ok(path)
}

/// Build the same control-file denies for project and granted write roots,
/// including a linked worktree's common Git directory. Backends that cannot
/// enforce nested denies report those paths as unenforced.
#[cfg(unix)]
fn checkout_control_denies(root: &Path, git: &GitPolicy) -> Result<Vec<PathBuf>, String> {
    let mut denies = Vec::new();
    let git_dirs = if git.read_roots.is_empty() {
        vec![root.join(".git")]
    } else {
        git.read_roots.clone()
    };
    for dir in git_dirs {
        denies.extend(["config", "config.worktree", "hooks"].map(|name| dir.join(name)));
    }
    denies.extend(git.hooks.iter().cloned());
    denies.extend(
        git.configured_paths
            .iter()
            .filter(|(key, _)| key != "core.excludesFile")
            .map(|(_, path)| path.clone()),
    );
    for key in ["core.hooksPath", "mason.originalHooksPath"] {
        if !git
            .configured_paths
            .iter()
            .any(|(existing, _)| existing == key)
        {
            if let Some((_, path)) = configured_git_path(root, key, &mut Vec::new())? {
                denies.push(path);
            }
        }
    }
    if let Some((git_dir, _)) = &git.linked {
        // Binding pointers must stay immutable too: redirecting one would
        // replace the operator-owned configuration used on the next spawn.
        denies.push(root.join(".git"));
        denies.extend(["commondir", "gitdir"].map(|name| git_dir.join(name)));
    }
    denies.extend([
        root.join(".cortexkit/aft.jsonc"),
        root.join(".cortexkit/aft/filters"),
    ]);
    Ok(denies)
}

/// Credential clients may use HOME, the password database, or explicit config
/// overrides. Include read/write denies for every location in both profiles,
/// without private-store exceptions. Backends enforce them or report their limits.
#[cfg(unix)]
fn add_credential_floor(
    profile: &mut SandboxProfile,
    home: &Path,
    roots: &[PathBuf],
) -> Result<(), String> {
    let account = crate::production_storage::account_home().ok();
    let (denies, cargo_homes) =
        credential_floor_paths(home, account.as_deref(), roots, |key| std::env::var_os(key))?;
    profile.read_deny.extend(denies.iter().cloned());
    profile.write_deny.extend(denies);
    add_relaxed_cargo_credential_denies(profile, cargo_homes)
}

#[cfg(unix)]
fn credential_floor_paths(
    home: &Path,
    account: Option<&Path>,
    roots: &[PathBuf],
    getenv: impl Fn(&str) -> Option<OsString>,
) -> Result<(Vec<PathBuf>, Vec<PathBuf>), String> {
    let mut denies = Vec::new();
    let mut cargo_homes = Vec::new();
    for home in std::iter::once(home).chain(account) {
        denies.extend(
            [
                ".ssh",
                ".gnupg",
                "Library/Keychains",
                ".aws",
                ".config/gcloud",
                ".azure",
                ".config/gh",
                ".netrc",
                ".docker/config.json",
                ".npmrc",
                ".pypirc",
                ".git-credentials",
                ".config/git/credentials",
                ".config/cortexkit",
            ]
            .map(|path| home.join(path)),
        );
        let (overrides, cargo) = relaxed_credential_paths(home, roots, &getenv)?;
        denies.extend(overrides);
        cargo_homes.extend(cargo);
    }
    denies.sort_unstable();
    denies.dedup();
    cargo_homes.sort_unstable();
    cargo_homes.dedup();
    Ok((denies, cargo_homes))
}

#[cfg(unix)]
fn add_relaxed_cargo_credential_denies(
    profile: &mut SandboxProfile,
    cargo_homes: Vec<PathBuf>,
) -> Result<(), String> {
    for cargo_home in cargo_homes {
        profile.deny_prefixes.extend(
            crate::sandbox_profile::resolve_env_deny_paths(
                cargo_home.join("credentials"),
                "CARGO_HOME",
            )
            .map_err(|error| error.to_string())?,
        );
        // Resolve existing prefix matches too: a credentials file can itself be a
        // symlink to a secret outside CARGO_HOME.
        match std::fs::read_dir(&cargo_home) {
            Ok(entries) => {
                for entry in entries {
                    let entry = match entry {
                        Ok(entry) => entry,
                        Err(error) => {
                            return Err(format!(
                                "cannot inspect CARGO_HOME credential entry: {error}"
                            ))
                        }
                    };
                    if !entry
                        .file_name()
                        .to_string_lossy()
                        .to_lowercase()
                        .starts_with("credentials")
                    {
                        continue;
                    }
                    let paths =
                        crate::sandbox_profile::resolve_env_deny_paths(entry.path(), "CARGO_HOME")
                            .map_err(|error| error.to_string())?;
                    profile.read_deny.extend(paths.iter().cloned());
                    profile.write_deny.extend(paths);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "cannot inspect CARGO_HOME credentials in {}: {error}",
                    cargo_home.display()
                ))
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn relaxed_credential_paths(
    home: &Path,
    roots: &[PathBuf],
    getenv: impl Fn(&str) -> Option<OsString>,
) -> Result<(Vec<PathBuf>, Vec<PathBuf>), String> {
    let errors = std::cell::RefCell::new(Vec::new());
    let paths = credential_paths_with_resolver(home, roots, getenv, |key, path| {
        match crate::sandbox_profile::resolve_env_deny_paths(path, key) {
            Ok(paths) => paths,
            Err(error) => {
                errors.borrow_mut().push(error.to_string());
                Vec::new()
            }
        }
    });
    let errors = errors.into_inner();
    if errors.is_empty() {
        Ok(paths)
    } else {
        Err(format!(
            "cannot resolve mandatory credential floor: {}",
            errors.join("; ")
        ))
    }
}

#[cfg(unix)]
fn credential_paths_with_resolver(
    home: &Path,
    roots: &[PathBuf],
    getenv: impl Fn(&str) -> Option<OsString>,
    resolve_path: impl Fn(&str, PathBuf) -> Vec<PathBuf>,
) -> (Vec<PathBuf>, Vec<PathBuf>) {
    // Relative tool settings are resolved from the shell's checkout, not the
    // daemon's working directory. Keep the default denies as well as overrides.
    let resolve = |key: &str, value: OsString, suffix: &str| {
        let path = PathBuf::from(value);
        let paths = if path.is_absolute() {
            vec![path]
        } else {
            roots.iter().map(|root| root.join(&path)).collect()
        };
        paths
            .into_iter()
            .flat_map(|path| {
                let path = if suffix.is_empty() {
                    path
                } else {
                    path.join(suffix)
                };
                resolve_path(key, path)
            })
            .collect::<Vec<_>>()
    };
    let paths = |key: &str, suffix: &str| {
        getenv(key)
            .filter(|value| !value.is_empty())
            .map(|value| resolve(key, value, suffix))
            .unwrap_or_default()
    };
    let mut cargo_homes = vec![home.join(".cargo")];
    cargo_homes.extend(paths("CARGO_HOME", ""));
    let mut denies = vec![home.join(".kube/config")];
    for key in [
        "GNUPGHOME",
        "GH_CONFIG_DIR",
        "CLOUDSDK_CONFIG",
        "AZURE_CONFIG_DIR",
    ] {
        denies.extend(paths(key, ""));
    }
    for key in ["AWS_CONFIG_FILE", "AWS_SHARED_CREDENTIALS_FILE"] {
        denies.extend(paths(key, ""));
    }
    denies.extend(paths("DOCKER_CONFIG", "config.json"));
    for suffix in ["gh", "gcloud", "git/credentials", "cortexkit"] {
        denies.extend(paths("XDG_CONFIG_HOME", suffix));
    }
    if let Some(value) = getenv("KUBECONFIG") {
        for path in std::env::split_paths(&value).filter(|path| !path.as_os_str().is_empty()) {
            denies.extend(resolve("KUBECONFIG", path.into_os_string(), ""));
        }
    }
    // RUSTUP_HOME holds toolchains and settings, not a credential store. Denying
    // it would disable the compiler without protecting authentication material.
    (denies, cargo_homes)
}

/// Prepare the shell environment after injecting managed Git/GitHub settings.
/// Strict workers clear rustc wrappers and SCCACHE_* channels: an external compile
/// server could write outside its client's write-limited jail. Relaxed and off
/// preserve compile-server addressing settings, not remote-cache credentials.
/// Relaxed shells and all workers keep the operator's CARGO_HOME/RUSTUP_HOME;
/// credential stripping happens separately in sandboxed_child_environment.
pub(crate) fn apply_worker_jail_environment(
    environment: &mut HashMap<String, String>,
    mode: SandboxMode,
) {
    if !worker_jail_active() && mode != SandboxMode::Relaxed {
        return;
    }
    if mode == SandboxMode::Strict {
        environment.retain(|key, _| !key.starts_with("SCCACHE_"));
        for key in ["RUSTC_WRAPPER", "CARGO_BUILD_RUSTC_WRAPPER"] {
            environment.insert(key.to_string(), String::new());
        }
    } else {
        for (key, value) in std::env::vars_os() {
            if let (Some(key), Some(value)) = (key.to_str(), value.to_str()) {
                if relaxed_compile_environment_key(key) {
                    environment
                        .entry(key.to_string())
                        .or_insert_with(|| value.to_string());
                }
            }
        }
    }
    #[cfg(unix)]
    for key in ["CARGO_HOME", "RUSTUP_HOME"] {
        if !environment.contains_key(key) {
            if let Some(path) = absolute_env_path(key) {
                environment.insert(key.to_string(), path.to_string_lossy().into_owned());
            }
        }
    }
}

fn relaxed_compile_environment_key(key: &str) -> bool {
    matches!(
        key,
        "SCCACHE_SERVER_PORT"
            | "SCCACHE_SERVER_UDS"
            | "SCCACHE_DIR"
            | "RUSTC_WRAPPER"
            | "CARGO_BUILD_RUSTC_WRAPPER"
    )
}

#[cfg(unix)]
fn sccache_server_ports(configured: Option<&str>) -> Vec<u16> {
    let mut ports = vec![4226];
    if let Some(port) = configured.and_then(|value| value.parse::<u16>().ok()) {
        if port != 0 && port != 4226 {
            ports.push(port);
        }
    }
    ports
}

/// What a jailed worker's shell may write outside its worktree.
#[cfg(unix)]
#[derive(Debug, Default, PartialEq, Eq)]
struct WorkerJailGrants {
    /// Package-manager and compiler caches (added to the cache roots).
    caches: Vec<PathBuf>,
    /// Directories outside the worktree: Git stores and operator build targets.
    roots: Vec<PathBuf>,
    /// Exact-file write grants for packed refs, Cargo cache tracking and
    /// build-slot locks, without granting their parent directories.
    files: Vec<PathBuf>,
    /// Cargo configuration, .rust-build-seeds, and immutable compiler launcher files.
    read_only: Vec<PathBuf>,
}

/// Cargo's lock and global-cache tracking files at the root of CARGO_HOME. The
/// directory itself also holds `bin` and `config.toml`, which stay read-only:
/// a shell that could rewrite them could run code outside the sandbox later.
#[cfg(unix)]
const CARGO_HOME_FILES: &[&str] = &[
    ".package-cache",
    ".package-cache-mutate",
    ".global-cache",
    ".global-cache-journal",
    ".global-cache-wal",
    ".global-cache-shm",
];

#[cfg(unix)]
fn absolute_env_path(name: &str) -> Option<PathBuf> {
    crate::environment::non_empty_os_var(name)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

/// What a jailed worker's shell may write outside its worktree. Every entry
/// comes from operator configuration: the AFT process environment, cargo
/// configuration files outside the worktree, and the standard cache homes.
/// Nothing comes from the command, its environment, or files inside the
/// worktree, which the model can edit.
///
/// - Git: a linked worktree's own Git directory, the shared object, ref and
///   log stores, and exactly `packed-refs`, `packed-refs.lock`, and
///   `packed-refs.new`. The shared
///   `config`, hooks, `info` and other worktrees' entries stay read-only.
/// - Cargo: `$CARGO_HOME/registry` and `git`, its lock files, and
///   `$RUSTUP_HOME/downloads` and `tmp`.
/// - Build targets named by `CARGO_TARGET_DIR` in the AFT environment or by
///   `build.target-dir` in cargo configuration outside the worktree, unless the
///   target is a protected path or would contain the home or the worktree.
#[cfg(unix)]
fn worker_jail_grants(
    home: &Path,
    project_roots: &[PathBuf],
    git_policies: &[GitPolicy],
    policy: &crate::access_policy::AccessPolicy,
) -> WorkerJailGrants {
    let mut grants = WorkerJailGrants {
        roots: linked_git_grants(git_policies),
        files: linked_git_file_grants(git_policies),
        ..WorkerJailGrants::default()
    };

    let cargo_home = absolute_env_path("CARGO_HOME").unwrap_or_else(|| home.join(".cargo"));
    if profile_candidate_is_dir(&cargo_home) {
        for cache in ["registry", "git"] {
            let cache = cargo_home.join(cache);
            if create_profile_candidate_dir(&cache) {
                grants.caches.push(cache);
            }
        }
        grants
            .files
            .extend(CARGO_HOME_FILES.iter().map(|name| cargo_home.join(name)));
    }
    let rustup_home = absolute_env_path("RUSTUP_HOME").unwrap_or_else(|| home.join(".rustup"));
    if profile_candidate_is_dir(&rustup_home) {
        for cache in ["downloads", "tmp"] {
            let cache = rustup_home.join(cache);
            if create_profile_candidate_dir(&cache) {
                grants.caches.push(cache);
            }
        }
    }

    // Cargo reads configuration from CARGO_HOME and from every ancestor of the
    // working directory. Only files outside the worktree are operator-owned.
    let mut configs = vec![
        (
            cargo_home.join("config.toml"),
            cargo_home.parent().map(Path::to_path_buf),
        ),
        (
            cargo_home.join("config"),
            cargo_home.parent().map(Path::to_path_buf),
        ),
    ];
    for root in project_roots {
        if let Some(container) = root.parent() {
            for path in [
                container.join(".cargo/alfonso-test-threads"),
                container.join(".rust-build-seeds"),
            ] {
                if let Ok(path) = path.canonicalize() {
                    if !project_roots.iter().any(|root| path.starts_with(root)) {
                        grants.read_only.push(path);
                    }
                }
            }
        }
        for ancestor in root.ancestors().skip(1) {
            for name in ["config.toml", "config"] {
                configs.push((
                    ancestor.join(".cargo").join(name),
                    Some(ancestor.to_path_buf()),
                ));
            }
        }
    }
    let mut targets = ["CARGO_TARGET_DIR", "CARGO_BUILD_TARGET_DIR"]
        .into_iter()
        .filter_map(absolute_env_path)
        .collect::<Vec<_>>();
    let mut seen = std::collections::BTreeSet::new();
    for (path, base) in configs {
        if !seen.insert(path.clone()) {
            continue;
        }
        let Ok(config_path) = canonicalize_profile_candidate(&path) else {
            continue;
        };
        if !profile_candidate_is_file(&config_path)
            || project_roots
                .iter()
                .any(|root| config_path.starts_with(root))
        {
            continue;
        }
        grants.read_only.push(config_path.clone());
        let Some(build) = cargo_build_settings(&config_path) else {
            continue;
        };
        if let Some(wrapper) = build.rustc_wrapper {
            let wrapper = PathBuf::from(wrapper);
            let wrapper = if wrapper.is_absolute() {
                Some(wrapper)
            } else {
                base.as_ref().map(|base| base.join(wrapper))
            };
            if let Some(wrapper) = wrapper {
                add_build_slot_grants(&wrapper, project_roots, policy, &mut grants);
            }
        }
        if let Some(target) = build.target_dir {
            let target = PathBuf::from(target);
            match (&base, target.is_absolute()) {
                (_, true) => targets.push(target),
                (Some(base), false) => targets.push(base.join(target)),
                (None, false) => {}
            }
        }
    }

    for target in targets {
        let target = crate::inspect::job::normalize_path(&target);
        if !jail_target_allowed(&target, home, project_roots, policy) {
            log::warn!(
                "worker sandbox: ignoring configured cargo target {}: it would contain the home, a project root, or a protected path",
                target.display()
            );
            continue;
        }
        if create_profile_candidate_dir(&target) {
            grants.roots.push(target);
        }
    }
    grants
}

/// Retain only the operator-configured `prefrontal-build-slot` launcher and its
/// `rustc-lowprio-direct` companion: they take a slot lock and compile inside
/// the sandbox rather than asking a shared server. The Cargo configuration and
/// launcher files must be outside every model-writable worktree.
#[cfg(unix)]
fn add_build_slot_grants(
    wrapper: &Path,
    project_roots: &[PathBuf],
    policy: &crate::access_policy::AccessPolicy,
    grants: &mut WorkerJailGrants,
) {
    let Ok(wrapper) = canonicalize_profile_candidate(wrapper) else {
        return;
    };
    if wrapper.file_name() != Some(OsStr::new("prefrontal-build-slot"))
        || !profile_candidate_is_file(&wrapper)
    {
        return;
    }
    let Some(dir) = wrapper.parent() else {
        return;
    };
    if project_roots
        .iter()
        .any(|root| root.starts_with(dir) || dir.starts_with(root))
        || !policy
            .private_stores
            .deny
            .iter()
            .any(|deny| dir.starts_with(deny) && dir != deny)
    {
        return;
    }
    let paths = [
        wrapper.clone(),
        dir.join("rustc-lowprio-direct"),
        dir.join(".root"),
        dir.join(".cache"),
        dir.join("limit"),
    ];
    let Ok(paths) = paths
        .into_iter()
        .map(|path| path.canonicalize())
        .collect::<Result<Vec<_>, _>>()
    else {
        return;
    };
    if paths
        .iter()
        .any(|path| !profile_candidate_is_file(path) || !path.starts_with(dir))
    {
        return;
    }
    let Some(limit) = std::fs::read_to_string(dir.join("limit"))
        .ok()
        .and_then(|value| value.trim().parse::<u16>().ok())
        .filter(|limit| (1..=256).contains(limit))
    else {
        return;
    };
    // Landlock's single-file grants require existing inodes. Provision only
    // the configured queue and slot locks, never a writable directory grant.
    // Include both endpoints so zero-based and one-based slot numbering work.
    let locks = std::iter::once(dir.join("queue.lock"))
        .chain((0..=limit).map(|slot| dir.join(format!("{slot}.lock"))));
    let mut files = Vec::new();
    for path in locks {
        if std::fs::symlink_metadata(&path)
            .is_ok_and(|metadata| !metadata.is_file() || metadata.file_type().is_symlink())
        {
            return;
        }
        if std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .is_err()
        {
            return;
        }
        files.push(path);
    }
    grants.files.extend(files);
    grants.read_only.extend(paths);
}

/// A linked worktree's own Git directory and its main repository's shared
/// object, ref and log stores: what `git add` and `git commit` write. The
/// shared `config`, hooks, `info` and other worktrees' entries are left out.
#[cfg(unix)]
fn linked_git_grants(git_policies: &[GitPolicy]) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for (git_dir, common_dir) in git_policies.iter().filter_map(|git| git.linked.as_ref()) {
        roots.push(git_dir.clone());
        roots.extend(
            ["objects", "refs", "logs"]
                .into_iter()
                .map(|store| common_dir.join(store))
                .filter(|store| profile_candidate_is_dir(store)),
        );
    }
    roots
}

/// Git locks packed refs and stages a rewritten table in `packed-refs.new`
/// before renaming it into place. Exact-file grants keep adjacent config and
/// hooks read-only, including when the lock and staging files do not exist yet.
#[cfg(unix)]
fn linked_git_file_grants(git_policies: &[GitPolicy]) -> Vec<PathBuf> {
    git_policies
        .iter()
        .filter_map(|git| git.linked.as_ref())
        .flat_map(|(_, common_dir)| {
            ["packed-refs", "packed-refs.lock", "packed-refs.new"].map(|name| common_dir.join(name))
        })
        .collect()
}

#[cfg(unix)]
#[derive(Debug, Default, PartialEq, Eq)]
struct CargoBuildSettings {
    target_dir: Option<String>,
    rustc_wrapper: Option<String>,
}

/// The `[build]` settings of one cargo configuration file, if it exists and parses.
#[cfg(unix)]
fn cargo_build_settings(path: &Path) -> Option<CargoBuildSettings> {
    let text = std::fs::read_to_string(path).ok()?;
    parse_cargo_build_settings(&text)
}

#[cfg(unix)]
fn parse_cargo_build_settings(text: &str) -> Option<CargoBuildSettings> {
    let table = text.parse::<toml::Table>().ok()?;
    let build = table.get("build")?.as_table()?;
    let field = |name: &str| {
        build
            .get(name)
            .and_then(|value| value.as_str())
            .map(str::to_string)
    };
    Some(CargoBuildSettings {
        target_dir: field("target-dir"),
        rustc_wrapper: field("rustc-wrapper"),
    })
}

/// A configured build target may not contain the home or a project root
/// (granting it would grant them), and may not be a protected path.
#[cfg(unix)]
fn jail_target_allowed(
    target: &Path,
    home: &Path,
    project_roots: &[PathBuf],
    policy: &crate::access_policy::AccessPolicy,
) -> bool {
    use crate::access_policy::{AccessDecision, AccessDenial};
    if !target.is_absolute() || target.parent().is_none() {
        return false;
    }
    let target = crate::access_policy::policy_path(target);
    !home.starts_with(&target)
        && !project_roots.iter().any(|root| root.starts_with(&target))
        && policy.decide_tree_write(&target) != AccessDecision::Deny(AccessDenial::Floor)
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
#[derive(Clone, Debug)]
struct GitPolicy {
    read_roots: Vec<PathBuf>,
    hooks: Vec<PathBuf>,
    /// For a linked worktree: its own Git directory (`<common>/worktrees/<name>`)
    /// and the shared common directory of the main repository.
    linked: Option<(PathBuf, PathBuf)>,
    configured_paths: Vec<(String, PathBuf)>,
    metadata_allow: Vec<PathBuf>,
}

#[cfg(unix)]
#[derive(Clone, Hash, Eq, PartialEq)]
struct GitPolicyCacheKey {
    worktree: PathBuf,
    git_dir: PathBuf,
    common_dir: PathBuf,
    environment: Vec<(OsString, OsString)>,
    worker: bool,
}

#[cfg(unix)]
const GIT_POLICY_CACHE_CAPACITY: usize = 256;

#[cfg(unix)]
#[derive(Clone)]
struct CachedGitPolicy {
    policy: GitPolicy,
    inputs: GitPolicyInputs,
}

#[cfg(unix)]
struct GitPolicyCache {
    entries: HashMap<GitPolicyCacheKey, Arc<CachedGitPolicy>>,
    least_recent: VecDeque<GitPolicyCacheKey>,
    in_flight: HashSet<GitPolicyCacheKey>,
}

#[cfg(unix)]
struct GitPolicyFlightGuard {
    key: GitPolicyCacheKey,
}

#[cfg(unix)]
impl Drop for GitPolicyFlightGuard {
    fn drop(&mut self) {
        let mut cache = git_policy_cache()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        cache.in_flight.remove(&self.key);
        #[cfg(test)]
        with_git_policy_test_state(&self.key.worktree, |state| {
            if state.expire_wait_on_owner_release {
                state.expire_wait = true;
            }
        });
        drop(cache);
        git_policy_cache_changed().notify_all();
    }
}

#[cfg(unix)]
impl GitPolicyCache {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
            least_recent: VecDeque::new(),
            in_flight: HashSet::new(),
        }
    }

    fn get(&mut self, key: &GitPolicyCacheKey) -> Option<Arc<CachedGitPolicy>> {
        let cached = self.entries.get(key).cloned()?;
        self.least_recent.retain(|candidate| candidate != key);
        self.least_recent.push_front(key.clone());
        Some(cached)
    }

    fn remove(&mut self, key: &GitPolicyCacheKey) {
        self.entries.remove(key);
        self.least_recent.retain(|candidate| candidate != key);
    }

    fn remove_if_same(&mut self, key: &GitPolicyCacheKey, cached: &Arc<CachedGitPolicy>) {
        if self
            .entries
            .get(key)
            .is_some_and(|current| Arc::ptr_eq(current, cached))
        {
            self.remove(key);
        }
    }

    fn insert(&mut self, key: GitPolicyCacheKey, cached: CachedGitPolicy) {
        self.remove(&key);
        self.least_recent.push_front(key.clone());
        self.entries.insert(key, Arc::new(cached));
        while self.entries.len() > GIT_POLICY_CACHE_CAPACITY {
            let Some(expired) = self.least_recent.pop_back() else {
                break;
            };
            self.entries.remove(&expired);
        }
    }
}

#[cfg(unix)]
fn git_policy_cache() -> &'static Mutex<GitPolicyCache> {
    static CACHE: OnceLock<Mutex<GitPolicyCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(GitPolicyCache::new()))
}

#[cfg(unix)]
fn git_policy_cache_changed() -> &'static Condvar {
    static CHANGED: OnceLock<Condvar> = OnceLock::new();
    CHANGED.get_or_init(Condvar::new)
}

#[cfg(all(test, unix))]
const GIT_POLICY_FLIGHT_WAIT_TIMEOUT: Duration = Duration::from_secs(2);
#[cfg(all(not(test), unix))]
const GIT_POLICY_FLIGHT_WAIT_TIMEOUT: Duration = Duration::from_secs(5);

#[cfg(all(test, unix))]
struct GitPolicyTestUncachedGate {
    entered: std::sync::mpsc::Sender<()>,
    release: std::sync::mpsc::Receiver<bool>,
}

#[cfg(all(test, unix))]
#[derive(Default)]
struct GitPolicyTestState {
    environment: Vec<(OsString, OsString)>,
    calls: usize,
    delay_ms: u64,
    uncached_gate: Option<GitPolicyTestUncachedGate>,
    waiter_signal: Option<std::sync::mpsc::Sender<()>>,
    expire_wait: bool,
    expire_wait_on_owner_release: bool,
    after_config_listing: Option<Box<dyn FnOnce() + Send>>,
}

#[cfg(all(test, unix))]
fn git_policy_test_states() -> &'static Mutex<HashMap<PathBuf, GitPolicyTestState>> {
    static STATES: OnceLock<Mutex<HashMap<PathBuf, GitPolicyTestState>>> = OnceLock::new();
    STATES.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(all(test, unix))]
fn with_git_policy_test_state<R>(
    project_root: &Path,
    run: impl FnOnce(&mut GitPolicyTestState) -> R,
) -> Option<R> {
    // Aliases of a fixture share its state; unrelated roots never see it.
    let root = project_root.canonicalize().ok()?;
    git_policy_test_states()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get_mut(&root)
        .map(run)
}

#[cfg(all(test, unix))]
fn signal_git_policy_waiter_for_test(project_root: &Path) {
    let signal =
        with_git_policy_test_state(project_root, |state| state.waiter_signal.take()).flatten();
    if let Some(signal) = signal {
        let _ = signal.send(());
    }
}

#[cfg(unix)]
fn git_policy_wait_remaining(_project_root: &Path, deadline: Instant) -> Duration {
    #[cfg(test)]
    if with_git_policy_test_state(_project_root, |state| state.expire_wait).unwrap_or(false) {
        return Duration::ZERO;
    }
    deadline.saturating_duration_since(Instant::now())
}

#[cfg(unix)]
#[derive(Clone, Debug, Eq, PartialEq)]
struct GitInputIdentity {
    device: u64,
    inode: u64,
    symlink: bool,
    // Directory contents (index.lock, refs, etc.) change on ordinary Git use.
    // Only a directory replacement affects config lookup, not its mtime.
    contents: Option<(u64, i64, i64, i64, i64)>,
}

#[cfg(unix)]
fn git_input_identity(path: &Path) -> std::io::Result<Option<GitInputIdentity>> {
    use std::os::unix::fs::MetadataExt;
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(GitInputIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
            symlink: metadata.file_type().is_symlink(),
            contents: (!metadata.is_dir()).then(|| {
                (
                    metadata.len(),
                    metadata.mtime(),
                    metadata.mtime_nsec(),
                    metadata.ctime(),
                    metadata.ctime_nsec(),
                )
            }),
        })),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
#[derive(Clone)]
struct GitPolicyInputs {
    environment: Vec<(OsString, OsString)>,
    executable: PathBuf,
    files: BTreeMap<PathBuf, Option<GitInputIdentity>>,
    resolved_paths: Vec<(PathBuf, PathBuf)>,
}

#[cfg(unix)]
impl GitPolicyInputs {
    fn watch(&mut self, path: &Path) -> Option<()> {
        // Include parent identities: a config symlink or any ancestor can be
        // retargeted without changing the inode of the previously read target.
        for ancestor in path.ancestors() {
            self.files
                .entry(ancestor.to_path_buf())
                .or_insert(git_input_identity(ancestor).ok()?);
        }
        let target = match path.canonicalize() {
            Ok(target) => target,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return (!path.ancestors().any(|ancestor| {
                    self.files
                        .get(ancestor)
                        .and_then(Option::as_ref)
                        .is_some_and(|identity| identity.symlink)
                }))
                .then_some(());
            }
            Err(_) => return None,
        };
        if target != path {
            for ancestor in target.ancestors() {
                self.files
                    .entry(ancestor.to_path_buf())
                    .or_insert(git_input_identity(ancestor).ok()?);
            }
            let resolved = (path.to_path_buf(), target);
            if !self.resolved_paths.contains(&resolved) {
                self.resolved_paths.push(resolved);
            }
        }
        Some(())
    }

    fn unchanged(&self, project_root: &Path) -> bool {
        self.environment == git_policy_environment(project_root)
            && git_policy_executable().as_ref() == Some(&self.executable)
            && self.files.iter().all(|(path, identity)| {
                git_input_identity(path).is_ok_and(|current| current == *identity)
            })
            && self.resolved_paths.iter().all(|(path, expected)| {
                canonicalize_policy_path(path.clone(), "cached Git policy")
                    .is_ok_and(|current| current == *expected)
            })
    }
}

#[cfg(unix)]
fn git_policy_environment(_project_root: &Path) -> Vec<(OsString, OsString)> {
    #[cfg(test)]
    if let Some(environment) =
        with_git_policy_test_state(_project_root, |state| state.environment.clone())
    {
        return environment;
    }
    let mut environment = std::env::vars_os()
        .filter(|(key, _)| {
            let key = key.to_string_lossy();
            key.starts_with("GIT_") || matches!(key.as_ref(), "HOME" | "XDG_CONFIG_HOME" | "PATH")
        })
        .collect::<Vec<_>>();
    environment.sort_unstable();
    environment
}

#[cfg(unix)]
fn git_policy_cache_key(
    project_root: &Path,
    environment: &[(OsString, OsString)],
    worker: bool,
) -> Option<GitPolicyCacheKey> {
    if environment.iter().any(|(key, _)| {
        matches!(
            key.to_str(),
            Some("GIT_DIR" | "GIT_COMMON_DIR" | "GIT_WORK_TREE")
        )
    }) {
        return None;
    }

    let dot_git = project_root.join(".git");
    let metadata = std::fs::symlink_metadata(&dot_git).ok()?;
    if metadata.file_type().is_symlink() {
        return None;
    }
    let git_dir = if metadata.is_dir() {
        dot_git.canonicalize().ok()?
    } else if metadata.is_file() {
        let pointer = std::fs::read_to_string(&dot_git).ok()?;
        let pointer = pointer.trim().strip_prefix("gitdir:")?.trim();
        if pointer.is_empty() {
            return None;
        }
        let pointer = PathBuf::from(pointer);
        let pointer = if pointer.is_absolute() {
            pointer
        } else {
            project_root.join(pointer)
        };
        pointer.canonicalize().ok()?
    } else {
        return None;
    };
    if !git_dir.is_dir() {
        return None;
    }

    let commondir = git_dir.join("commondir");
    let common_dir = match std::fs::read_to_string(&commondir) {
        Ok(value) => {
            let value = value.trim();
            if value.is_empty() {
                return None;
            }
            let value = PathBuf::from(value);
            let value = if value.is_absolute() {
                value
            } else {
                git_dir.join(value)
            };
            value.canonicalize().ok()?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => git_dir.clone(),
        Err(_) => return None,
    };
    if !common_dir.is_dir() {
        return None;
    }

    Some(GitPolicyCacheKey {
        worktree: project_root.canonicalize().ok()?,
        git_dir,
        common_dir,
        environment: environment.to_vec(),
        worker,
    })
}

#[cfg(unix)]
fn git_policy_executable() -> Option<PathBuf> {
    which::which_in(
        "git",
        Some(crate::effective_path::effective_path()),
        std::env::current_dir().ok()?,
    )
    .ok()
}

#[cfg(unix)]
fn git_policy_inputs(
    project_root: &Path,
    git_dir: &Path,
    common_dir: &Path,
    environment: Vec<(OsString, OsString)>,
) -> Option<GitPolicyInputs> {
    #[cfg(test)]
    if GIT_POLICY_BENCH_OLD.with(|value| value.get()) {
        return None;
    }
    if !crate::developer_tools::git_usable() {
        return None;
    }
    // These overrides redirect Git away from the .git/commondir topology whose
    // config files we track. Resolve normally, but never reuse that policy.
    if environment.iter().any(|(key, _)| {
        matches!(
            key.to_str(),
            Some("GIT_DIR" | "GIT_COMMON_DIR" | "GIT_WORK_TREE")
        )
    }) {
        return None;
    }
    let mut inputs = GitPolicyInputs {
        environment,
        executable: git_policy_executable()?,
        files: BTreeMap::new(),
        resolved_paths: Vec::new(),
    };
    // Git's compiled-in system-config location can change if its executable is
    // replaced, or another git appears earlier on PATH.
    inputs.watch(&inputs.executable.clone())?;
    let mut configs = vec![common_dir.join("config"), git_dir.join("config.worktree")];
    // Git knows its compiled-in system prefix and the global-file overrides.
    // Query the *candidate* files, not only --show-origin's existing files, so
    // creating a previously absent system/global config invalidates the cache.
    // Older Git without these variables remains correct but uncached.
    for variable in ["GIT_CONFIG_SYSTEM", "GIT_CONFIG_GLOBAL"] {
        let output = git_policy_output(project_root, &["var", variable]).ok()?;
        if !output.status.success() {
            return None;
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            for path in output
                .stdout
                .split(|byte| *byte == b'\n')
                .filter(|path| !path.is_empty())
            {
                let path = PathBuf::from(OsString::from_vec(path.to_vec()));
                configs.push(if path.is_absolute() {
                    path
                } else {
                    project_root.join(path)
                });
            }
        }
    }
    if let Some((_, path)) = inputs
        .environment
        .iter()
        .find(|(key, _)| key == "GIT_CONFIG")
    {
        let path = PathBuf::from(path);
        configs.push(if path.is_absolute() {
            path
        } else {
            project_root.join(path)
        });
    }
    inputs.watch(&project_root.join(".git"))?;
    inputs.watch(&git_dir.join("commondir"))?;
    // onbranch includes depend on this worktree's symbolic HEAD, not config.
    // Watch it unconditionally: one stat avoids parsing condition semantics.
    inputs.watch(&git_dir.join("HEAD"))?;
    for config in configs {
        inputs.watch(&config)?;
    }
    let discovered = git_policy_config_inputs(project_root, &mut inputs)?;
    // Includes are discovered after Git reads them. Repeat discovery with every
    // first-pass file already watched so an edit cannot hide a new dependency.
    let mut verified = inputs.clone();
    let rediscovered = git_policy_config_inputs(project_root, &mut verified)?;
    if discovered != rediscovered
        || inputs.files != verified.files
        || inputs.resolved_paths != verified.resolved_paths
        || !inputs.unchanged(project_root)
    {
        return None;
    }
    Some(inputs)
}

#[cfg(unix)]
fn git_policy_config_inputs(
    project_root: &Path,
    inputs: &mut GitPolicyInputs,
) -> Option<HashSet<PathBuf>> {
    let output = git_policy_output(
        project_root,
        &["config", "--includes", "--show-origin", "--null", "--list"],
    )
    .ok()?;
    if !output.status.success() {
        return None;
    }

    #[cfg(test)]
    if let Some(hook) =
        with_git_policy_test_state(project_root, |state| state.after_config_listing.take())
            .flatten()
    {
        hook();
    }

    let mut discovered = HashSet::new();
    let mut fields = output.stdout.split(|byte| *byte == 0);
    loop {
        let Some(origin) = fields.next() else {
            break;
        };
        if origin.is_empty() {
            break;
        }
        let record = fields.next()?;
        // With --null, a valueless boolean has no key/value newline at all.
        let (key, value) = match record.iter().position(|byte| *byte == b'\n') {
            Some(separator) => (&record[..separator], &record[separator + 1..]),
            None => (record, &b""[..]),
        };
        let origin_file = git_config_origin_path(project_root, origin);
        if let Some(path) = &origin_file {
            inputs.watch(path)?;
            discovered.insert(path.clone());
        }
        if key == b"include.path" || (key.starts_with(b"includeif.") && key.ends_with(b".path")) {
            if let Some(condition) = key.strip_prefix(b"includeif.") {
                // gitdir conditions use the watched Git-directory topology;
                // hasconfig uses remote URLs in these same config inputs (or
                // the watched environment). HEAD covers onbranch. New condition
                // types stay uncached until their dependencies are understood.
                if ![
                    b"gitdir:".as_slice(),
                    b"gitdir/i:",
                    b"onbranch:",
                    b"hasconfig:remote.*.url:",
                ]
                .iter()
                .any(|prefix| condition.starts_with(prefix))
                {
                    return None;
                }
            }
            let origin_file = origin_file?;
            let include = git_include_path(project_root, &origin_file, value, &inputs.environment)?;
            inputs.watch(&include)?;
            discovered.insert(include);
        }
    }
    Some(discovered)
}

#[cfg(unix)]
fn git_config_origin_path(project_root: &Path, origin: &[u8]) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;

    let path = origin.strip_prefix(b"file:")?;
    if path.is_empty() {
        return None;
    }
    let path = PathBuf::from(OsString::from_vec(path.to_vec()));
    Some(if path.is_absolute() {
        path
    } else {
        project_root.join(path)
    })
}

#[cfg(unix)]
fn git_include_path(
    project_root: &Path,
    origin: &Path,
    value: &[u8],
    environment: &[(OsString, OsString)],
) -> Option<PathBuf> {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    let value = OsString::from_vec(value.to_vec());
    let value_path = PathBuf::from(&value);
    if value_path.is_absolute() {
        return Some(value_path);
    }
    if let Some(value) = value.to_str() {
        if let Some(suffix) = value.strip_prefix('~') {
            if !suffix.is_empty() && !suffix.starts_with('/') {
                return None;
            }
            let home = environment
                .iter()
                .find(|(key, _)| key == "HOME")
                .map(|(_, value)| PathBuf::from(value))?;
            return Some(home.join(suffix.trim_start_matches('/')));
        }
    } else if value.as_bytes().starts_with(b"~") {
        return None;
    }
    origin.parent().map(|parent| {
        let path = parent.join(value_path);
        if path.is_absolute() {
            path
        } else {
            project_root.join(path)
        }
    })
}

#[cfg(all(test, unix))]
thread_local! {
    static GIT_POLICY_BENCH_OLD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(unix)]
fn git_policy_output(project_root: &Path, args: &[&str]) -> std::io::Result<std::process::Output> {
    #[cfg(test)]
    {
        let delay = with_git_policy_test_state(project_root, |state| {
            state.calls += 1;
            state.delay_ms
        })
        .unwrap_or(0);
        if delay > 0 {
            std::thread::sleep(Duration::from_millis(delay));
        }
    }
    let command = crate::effective_path::new_command("git");
    #[cfg(test)]
    let mut command = command;
    #[cfg(test)]
    if GIT_POLICY_BENCH_OLD.with(|value| value.get()) {
        command.env("PATH", crate::effective_path::effective_path());
    }
    #[cfg(test)]
    {
        if let Some(environment) =
            with_git_policy_test_state(project_root, |state| state.environment.clone())
        {
            for (key, _) in
                std::env::vars_os().filter(|(key, _)| key.to_string_lossy().starts_with("GIT_"))
            {
                command.env_remove(key);
            }
            command.envs(environment.iter().cloned());
        }
    }
    #[cfg(test)]
    let mut command = if GIT_POLICY_BENCH_OLD.with(|value| value.get()) {
        command.build_unresolved()
    } else {
        command.build()
    };
    #[cfg(not(test))]
    let mut command = command.build();
    command.arg("-C").arg(project_root).args(args).output()
}

#[cfg(unix)]
fn resolve_git_policy(project_root: &Path) -> Result<GitPolicy, String> {
    crate::bash_background::enter_bash_startup_step(
        crate::bash_background::BashStartupStep::GitPolicy,
    );
    let result = resolve_git_policy_inner(project_root);
    crate::bash_background::enter_bash_startup_step(
        crate::bash_background::BashStartupStep::Profile,
    );
    result
}

#[cfg(unix)]
fn resolve_git_policy_inner(project_root: &Path) -> Result<GitPolicy, String> {
    let environment = git_policy_environment(project_root);
    let worker = worker_jail_active();
    let Some(key) = git_policy_cache_key(project_root, &environment, worker) else {
        return resolve_git_policy_uncached(project_root, environment, worker)
            .map(|(policy, _)| policy);
    };
    let wait_deadline = Instant::now() + GIT_POLICY_FLIGHT_WAIT_TIMEOUT;

    loop {
        let (cached, flight_guard) = {
            let mut cache = git_policy_cache()
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            loop {
                if let Some(cached) = cache.get(&key) {
                    break (Some(cached), None);
                }
                if !cache.in_flight.contains(&key) {
                    cache.in_flight.insert(key.clone());
                    break (None, Some(GitPolicyFlightGuard { key: key.clone() }));
                }
                // A wakeup or an elapsed deadline does not imply ownership.
                // Recheck the entry and flight under the lock before deciding
                // whether to reuse, become the next owner, or bypass a stalled
                // owner so its slow Git discovery does not block another call.
                let remaining = git_policy_wait_remaining(project_root, wait_deadline);
                if remaining.is_zero() {
                    drop(cache);
                    return resolve_git_policy_uncached(project_root, environment, worker)
                        .map(|(policy, _)| policy);
                }
                #[cfg(test)]
                signal_git_policy_waiter_for_test(project_root);
                let (next_cache, _) = git_policy_cache_changed()
                    .wait_timeout(cache, remaining)
                    .unwrap_or_else(|error| error.into_inner());
                cache = next_cache;
            }
        };

        if let Some(cached) = cached {
            if cached.inputs.unchanged(project_root) {
                return Ok(cached.policy.clone());
            }
            git_policy_cache()
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .remove_if_same(&key, &cached);
            continue;
        }
        let Some(flight_guard) = flight_guard else {
            continue;
        };

        // The in-flight marker serializes misses for this exact worktree and
        // input environment until a waiter's deadline. Filesystem checks and
        // Git subprocesses stay unlocked.
        let result = resolve_git_policy_uncached(project_root, environment.clone(), worker);
        let entry = result.as_ref().ok().and_then(|(policy, inputs)| {
            let current_key = git_policy_cache_key(project_root, &environment, worker);
            (current_key.as_ref() == Some(&key))
                .then(|| inputs.clone())
                .flatten()
                .filter(|inputs| inputs.unchanged(project_root))
                .map(|inputs| CachedGitPolicy {
                    policy: policy.clone(),
                    inputs,
                })
        });
        {
            let mut cache = git_policy_cache()
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if let Some(entry) = entry {
                cache.insert(key.clone(), entry);
            } else {
                cache.remove(&key);
            }
        }
        drop(flight_guard);
        return result.map(|(policy, _)| policy);
    }
}

#[cfg(unix)]
fn resolve_git_policy_uncached(
    project_root: &Path,
    environment: Vec<(OsString, OsString)>,
    worker: bool,
) -> Result<(GitPolicy, Option<GitPolicyInputs>), String> {
    #[cfg(test)]
    let test_gate =
        with_git_policy_test_state(project_root, |state| state.uncached_gate.take()).flatten();
    #[cfg(test)]
    if let Some(gate) = test_gate {
        let _ = gate.entered.send(());
        if gate.release.recv().unwrap() {
            panic!("test-injected panic during Git policy resolution");
        }
    }

    let dot_git = project_root.join(".git");
    let dot_git_identity = git_input_identity(&dot_git).ok();
    let metadata = match std::fs::symlink_metadata(&dot_git) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((
                GitPolicy {
                    read_roots: Vec::new(),
                    hooks: vec![dot_git.join("hooks")],
                    linked: None,
                    configured_paths: Vec::new(),
                    metadata_allow: Vec::new(),
                },
                None,
            ));
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
    let commondir_identity = git_input_identity(&commondir_file).ok();
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

    let mut inputs = git_policy_inputs(project_root, &git_dir, &common_dir, environment);
    if dot_git_identity != git_input_identity(&dot_git).ok()
        || commondir_identity != git_input_identity(&commondir_file).ok()
    {
        inputs = None;
    }
    let (hooks_input, hooks) = resolve_hooks_path(project_root, &common_dir)?;
    let mut metadata_allow = Vec::new();
    let mut configured_paths = Vec::new();
    for key in if worker {
        &[
            "core.hooksPath",
            "mason.originalHooksPath",
            "core.excludesFile",
        ][..]
    } else {
        &[]
    } {
        if let Some((input, path)) = configured_git_path(project_root, key, &mut metadata_allow)? {
            if let Some(inputs) = &mut inputs {
                inputs.resolved_paths.push((input, path.clone()));
            }
            configured_paths.push((key.to_string(), path));
        }
    }
    // Re-resolve canonical paths on hits so creation of, or changes to, symlinks
    // in hooks/excludes paths cannot leave a deny pointing at the old target.
    if let Some(inputs) = &mut inputs {
        inputs.resolved_paths.push((hooks_input, hooks.clone()));
    }
    if !metadata_allow.is_empty() {
        inputs = None;
    }
    let linked = (git_dir != common_dir).then(|| (git_dir.clone(), common_dir.clone()));
    let read_roots = {
        let mut read_roots = vec![git_dir, common_dir];
        read_roots.sort_unstable();
        read_roots.dedup();
        read_roots
    };
    Ok((
        GitPolicy {
            read_roots,
            hooks: vec![hooks],
            linked,
            configured_paths,
            metadata_allow,
        },
        inputs,
    ))
}

#[cfg(unix)]
fn resolve_hooks_path(
    project_root: &Path,
    common_dir: &Path,
) -> Result<(PathBuf, PathBuf), String> {
    // Without runnable git nothing can read core.hooksPath, and nothing can
    // run a hook either; deny the default hooks directory, which is what an
    // unset core.hooksPath resolves to.
    if !crate::developer_tools::git_usable() {
        return resolve_git_policy_path(common_dir.join("hooks"), "Git hooks");
    }
    let configured = git_policy_output(project_root, &["config", "--path", "core.hooksPath"])
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
        let resolved = git_policy_output(
            project_root,
            &["rev-parse", "--path-format=absolute", "--git-path", "hooks"],
        )
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
        return resolve_git_policy_path(resolved, "core.hooksPath");
    }

    if configured.status.code() != Some(1) || !configured.stdout.is_empty() {
        return Err(format!(
            "git could not query core.hooksPath for {}: {}",
            project_root.display(),
            String::from_utf8_lossy(&configured.stderr).trim()
        ));
    }
    resolve_git_policy_path(common_dir.join("hooks"), "Git hooks")
}

/// Resolve operator-owned Git path settings before any command-supplied
/// environment is applied. `--path` expands Git's home-relative spelling.
#[cfg(unix)]
fn configured_git_path(
    project_root: &Path,
    key: &str,
    metadata_allow: &mut Vec<PathBuf>,
) -> Result<Option<(PathBuf, PathBuf)>, String> {
    if !project_root.join(".git").exists() || !crate::developer_tools::git_usable() {
        return Ok(None);
    }
    let worktree_config = git_policy_output(
        project_root,
        &["config", "--local", "--bool", "extensions.worktreeConfig"],
    )
    .map_err(|error| format!("failed to query worktree config extension: {error}"))?;
    let worktree_config = worktree_config.status.success() && worktree_config.stdout == b"true\n";
    // AFT injects core.hooksPath through command-scope GIT_CONFIG_* settings.
    // Its hooks then query repository scopes to run the configured policy.
    // Resolve those same scopes so the injected path cannot hide that policy.
    for scope in ["--worktree", "--local", "--global", "--system"] {
        if scope == "--worktree" && !worktree_config {
            continue;
        }
        let output = git_policy_output(project_root, &["config", scope, "--path", "--get", key])
            .map_err(|error| format!("failed to query {key}: {error}"))?;
        if output.status.code() == Some(1) && output.stdout.is_empty() {
            continue;
        }
        if !output.status.success() {
            return Err(format!(
                "failed to query {key}: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        let value = String::from_utf8(output.stdout)
            .map_err(|error| format!("{key} is not UTF-8: {error}"))?;
        let path = PathBuf::from(value.trim());
        if !path.as_os_str().is_empty() {
            let path = if path.is_absolute() {
                path
            } else {
                project_root.join(path)
            };
            for ancestor in path.ancestors() {
                if std::fs::symlink_metadata(ancestor)
                    .is_ok_and(|metadata| metadata.file_type().is_symlink())
                {
                    metadata_allow.push(ancestor.to_path_buf());
                }
            }
            return resolve_git_policy_path(path, key).map(Some);
        }
    }
    Ok(None)
}

#[cfg(unix)]
fn resolve_git_policy_path(path: PathBuf, field: &str) -> Result<(PathBuf, PathBuf), String> {
    canonicalize_policy_path(path.clone(), field).map(|resolved| (path, resolved))
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
    fn children(&mut self, parent: &Path) -> std::io::Result<Vec<ListedReadChild>>;
}

#[cfg(all(test, target_os = "linux"))]
mod fixed_read_inventory {
    use super::*;

    type Inventory = Arc<BTreeMap<PathBuf, Vec<ListedReadChild>>>;

    thread_local! {
        static CURRENT: RefCell<Option<Inventory>> = const { RefCell::new(None) };
    }

    pub(super) struct Scope {
        previous: Option<Inventory>,
        _same_thread: std::marker::PhantomData<std::rc::Rc<()>>,
    }

    impl Drop for Scope {
        fn drop(&mut self) {
            CURRENT.with(|slot| slot.replace(self.previous.take()));
        }
    }

    pub(super) fn install(root: &Path) -> Scope {
        fn capture(
            root: &Path,
            directories: &mut BTreeMap<PathBuf, Vec<ListedReadChild>>,
        ) -> std::io::Result<()> {
            let children = SecureReadDirectoryLister.children(root)?;
            for child in &children {
                if child.is_dir {
                    capture(&child.path, directories)?;
                }
            }
            directories.insert(root.to_path_buf(), children);
            Ok(())
        }

        let root = root.canonicalize().unwrap();
        let mut directories = BTreeMap::new();
        capture(&root, &mut directories).expect("capture the immutable profile fixture");
        // This fixture's filesystem contains its own entries and the ancestor
        // chain needed to reach them, not other tests' home or temp directories.
        for child in root.ancestors() {
            if let Some(parent) = child.parent() {
                directories
                    .entry(parent.to_path_buf())
                    .or_default()
                    .push(ListedReadChild {
                        path: child.to_path_buf(),
                        is_dir: true,
                    });
            }
        }
        let previous = CURRENT.with(|slot| slot.replace(Some(Arc::new(directories))));
        Scope {
            previous,
            _same_thread: std::marker::PhantomData,
        }
    }

    pub(super) struct FixtureLister(Inventory);

    impl ReadDirectoryLister for FixtureLister {
        fn children(&mut self, parent: &Path) -> std::io::Result<Vec<ListedReadChild>> {
            // An unlisted directory is empty in this fixed fixture universe.
            Ok(self.0.get(parent).cloned().unwrap_or_default())
        }
    }

    pub(super) fn current() -> Option<FixtureLister> {
        CURRENT.with(|slot| slot.borrow().clone().map(FixtureLister))
    }
}

#[cfg(all(test, unix))]
mod candidate_snapshot {
    // Both policies make their own decisions against the same optional-path
    // facts, including file/dir kind, canonicalization and creation outcomes.
    // No computed profile fields or comparison results are cached here.
    use super::*;

    struct ObservedPath {
        exists: bool,
        directory: bool,
        file: bool,
        canonical: Result<PathBuf, (std::io::ErrorKind, String)>,
    }

    struct Snapshot {
        candidates: [PathBuf; 2],
        paths: Mutex<BTreeMap<PathBuf, Arc<ObservedPath>>>,
        created: Mutex<BTreeMap<PathBuf, bool>>,
    }

    impl Snapshot {
        fn path(&self, path: &Path) -> Arc<ObservedPath> {
            self.paths
                .lock()
                .unwrap()
                .entry(path.to_path_buf())
                .or_insert_with(|| {
                    let metadata = std::fs::metadata(path);
                    let canonical = match &metadata {
                        Ok(_) => path.canonicalize(),
                        Err(error) => Err(std::io::Error::new(error.kind(), error.to_string())),
                    }
                    .map_err(|error| (error.kind(), error.to_string()));
                    Arc::new(ObservedPath {
                        exists: metadata.is_ok(),
                        directory: metadata.as_ref().is_ok_and(|value| value.is_dir()),
                        file: metadata.as_ref().is_ok_and(|value| value.is_file()),
                        canonical,
                    })
                })
                .clone()
        }
    }

    thread_local! {
        static CURRENT: RefCell<Option<Arc<Snapshot>>> = const { RefCell::new(None) };
    }

    pub(super) struct Scope {
        previous: Option<Arc<Snapshot>>,
        _same_thread: std::marker::PhantomData<std::rc::Rc<()>>,
    }

    impl Drop for Scope {
        fn drop(&mut self) {
            CURRENT.with(|slot| slot.replace(self.previous.take()));
        }
    }

    pub(super) fn install(root: &Path) -> Scope {
        let snapshot = Arc::new(Snapshot {
            candidates: [root.join("fixed-cache-marker"), root.join(".cargo/git")],
            paths: Mutex::new(BTreeMap::new()),
            created: Mutex::new(BTreeMap::new()),
        });
        for path in &snapshot.candidates {
            snapshot.path(path);
        }
        let previous = CURRENT.with(|slot| slot.replace(Some(snapshot)));
        Scope {
            previous,
            _same_thread: std::marker::PhantomData,
        }
    }

    fn current() -> Option<Arc<Snapshot>> {
        CURRENT.with(|slot| slot.borrow().clone())
    }

    pub(super) fn cache_candidates() -> Vec<PathBuf> {
        current()
            .map(|snapshot| snapshot.candidates.to_vec())
            .unwrap_or_default()
    }

    pub(super) fn is_dir(path: &Path) -> Option<bool> {
        current().map(|snapshot| snapshot.path(path).directory)
    }

    pub(super) fn is_file(path: &Path) -> Option<bool> {
        current().map(|snapshot| snapshot.path(path).file)
    }

    pub(super) fn exists(path: &Path) -> Option<bool> {
        current().map(|snapshot| snapshot.path(path).exists)
    }

    pub(super) fn canonicalize(path: &Path) -> Option<std::io::Result<PathBuf>> {
        current().map(|snapshot| match &snapshot.path(path).canonical {
            Ok(path) => Ok(path.clone()),
            Err((kind, message)) => Err(std::io::Error::new(*kind, message.clone())),
        })
    }

    pub(super) fn create_dir(path: &Path) -> Option<bool> {
        current().map(|snapshot| {
            let mut created = snapshot.created.lock().unwrap();
            *created
                .entry(path.to_path_buf())
                .or_insert_with(|| std::fs::create_dir_all(path).is_ok())
        })
    }
}

// Optional policy inputs share one observation in the parity fixture. Required
// project/task roots and production launches still use their normal validation.
#[cfg(unix)]
fn profile_candidate_is_dir(path: &Path) -> bool {
    #[cfg(test)]
    if let Some(value) = candidate_snapshot::is_dir(path) {
        return value;
    }
    path.is_dir()
}

#[cfg(any(unix, test))]
fn profile_candidate_is_file(path: &Path) -> bool {
    #[cfg(all(test, unix))]
    if let Some(value) = candidate_snapshot::is_file(path) {
        return value;
    }
    path.is_file()
}

#[cfg(unix)]
fn profile_candidate_exists(path: &Path) -> bool {
    #[cfg(test)]
    if let Some(value) = candidate_snapshot::exists(path) {
        return value;
    }
    path.exists()
}

#[cfg(unix)]
fn canonicalize_profile_candidate(path: &Path) -> std::io::Result<PathBuf> {
    #[cfg(test)]
    if let Some(value) = candidate_snapshot::canonicalize(path) {
        return value;
    }
    path.canonicalize()
}

#[cfg(unix)]
fn create_profile_candidate_dir(path: &Path) -> bool {
    #[cfg(test)]
    if let Some(value) = candidate_snapshot::create_dir(path) {
        return value;
    }
    std::fs::create_dir_all(path).is_ok()
}

#[cfg(target_os = "linux")]
fn build_linux_profile(
    profile: SandboxProfile,
    home: &Path,
    git_read_roots: &[PathBuf],
) -> Result<SandboxProfile, String> {
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

    #[cfg(test)]
    if let Some(mut inventory) = fixed_read_inventory::current() {
        return compute_linux_profile(profile, home, git_read_roots, &intended, &mut inventory);
    }

    compute_linux_profile(
        profile,
        home,
        git_read_roots,
        &intended,
        &mut SecureReadDirectoryLister,
    )
}

/// Compute the exact launcher policy from canonical paths and an injected
/// directory inventory. Only the wrapper above discovers host filesystem paths;
/// this policy calculation also runs in tests on hosts without Landlock.
#[cfg(any(test, target_os = "linux"))]
fn compute_linux_profile(
    mut profile: SandboxProfile,
    home: &Path,
    git_read_roots: &[PathBuf],
    base_read_grants: &[IntendedReadGrant],
    lister: &mut impl ReadDirectoryLister,
) -> Result<SandboxProfile, String> {
    validate_mandatory_floor_overlap(profile.write_allow_roots(), &profile.write_deny)?;
    profile
        .data_policy
        .validate_grants(
            profile.write_allow_roots(),
            &profile.data_policy.write_allow,
            &profile.writable_files,
        )
        .map_err(|error| error.to_string())?;

    let mut intended = base_read_grants.to_vec();

    intended.push(IntendedReadGrant {
        path: home.to_path_buf(),
        force_children: true,
        mandatory: false,
    });
    intended.extend(
        profile
            .write_allow_roots()
            .into_iter()
            .chain(
                profile
                    .writable_files
                    .iter()
                    .map(PathBuf::as_path)
                    .filter(|file| profile_candidate_is_file(file)),
            )
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
        .chain(&profile.data_policy.deny)
        .cloned()
        .collect::<Vec<_>>();
    let (mut grants, mut listed) =
        split_read_grants_with_inventory(&intended, &split_denies, lister)?;
    // Reintroduce only explicit private-tree exceptions, still split around
    // ordinary denies (including the connection file and repository hooks).
    // A nested repository-hooks deny means reads are granted per existing
    // project child. Files newly created at the project root after this launch
    // cannot be read within the same sandboxed command; writes remain allowed.
    let exceptions = profile
        .data_policy
        .read_allow
        .iter()
        .filter(|path| !profile.writable_files.contains(path) || profile_candidate_is_file(path))
        .map(|path| IntendedReadGrant {
            path: path.clone(),
            force_children: false,
            mandatory: false,
        })
        .collect::<Vec<_>>();
    let (exception_grants, exception_listed) =
        split_read_grants_with_inventory(&exceptions, &profile.read_deny, lister)?;
    // Direct and explicitly supplied grants remain required even if the same
    // path was also discovered while splitting a different read root.
    let required = grants
        .iter()
        .filter(|path| !listed.contains(path))
        .chain(
            exception_grants
                .iter()
                .filter(|path| !exception_listed.contains(path)),
        )
        .chain(&profile.read_allow)
        .cloned()
        .collect::<BTreeSet<_>>();
    grants.extend(exception_grants);
    grants.extend(profile.read_allow);
    listed.extend(exception_listed);
    listed.retain(|path| !required.contains(path));
    listed.sort_unstable();
    listed.dedup();
    grants.sort_unstable();
    grants.dedup();
    profile
        .data_policy
        .validate_grants(
            grants.iter().map(PathBuf::as_path),
            &profile.data_policy.read_allow,
            &profile.writable_files,
        )
        .map_err(|error| error.to_string())?;
    profile.read_allow = grants;
    profile.read_allow_listed = listed;
    Ok(profile)
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
    // Payload files are exact, daemon-verified objects, not directory grants.
    profile
        .data_policy
        .read_allow
        .extend(payload_grants.iter().cloned());

    let mut final_read_allow = profile.read_allow.clone();
    profile
        .read_allow_listed
        .retain(|path| !payload_grants.contains(path));
    final_read_allow.extend(payload_grants);
    final_read_allow.sort_unstable();
    final_read_allow.dedup();
    validate_final_read_rules(&final_read_allow, &profile.read_deny)?;
    profile
        .data_policy
        .validate_grants(
            final_read_allow.iter().map(PathBuf::as_path),
            &profile.data_policy.read_allow,
            &profile.writable_files,
        )
        .map_err(|error| error.to_string())?;
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
    split_read_grants_with_inventory(intended, denies, lister).map(|(paths, _)| paths)
}

#[cfg(any(test, target_os = "linux"))]
fn split_read_grants_with_inventory(
    intended: &[IntendedReadGrant],
    denies: &[PathBuf],
    lister: &mut impl ReadDirectoryLister,
) -> Result<(Vec<PathBuf>, Vec<PathBuf>), String> {
    let mut emitted = BTreeMap::new();
    for grant in intended {
        split_read_grant(grant, denies, lister, false, &mut emitted)?;
    }
    let paths = emitted.keys().cloned().collect::<Vec<_>>();
    let listed = emitted
        .into_iter()
        .filter_map(|(path, listed)| listed.then_some(path))
        .collect();
    validate_final_read_rules(&paths, denies)?;
    Ok((paths, listed))
}

#[cfg(any(test, target_os = "linux"))]
fn split_read_grant(
    grant: &IntendedReadGrant,
    denies: &[PathBuf],
    lister: &mut impl ReadDirectoryLister,
    from_listing: bool,
    emitted: &mut BTreeMap<PathBuf, bool>,
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
        emitted
            .entry(grant.path.clone())
            .and_modify(|listed| *listed &= from_listing)
            .or_insert(from_listing);
        return Ok(());
    }

    let children = match lister.children(&grant.path) {
        Ok(children) => children,
        Err(error) if from_listing && error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(format!(
                "sandbox_unavailable: cannot split read root {}: {error}",
                grant.path.display()
            ))
        }
    };
    for child in children {
        // An exact denied file is simply omitted by the recursive call. Only a
        // strict descendant deny would require traversing this child as a directory.
        let child_contains_deny = denies
            .iter()
            .any(|deny| deny != &child.path && deny.starts_with(&child.path));
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
            true,
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
    fn children(&mut self, parent: &Path) -> std::io::Result<Vec<ListedReadChild>> {
        let parent_fd = open_absolute_no_symlinks(parent, true)?;
        let readable_fd =
            open_directory_for_enumeration(parent_fd.as_raw_fd()).map_err(|error| {
                std::io::Error::new(
                    error.kind(),
                    format!(
                        "failed to open directory for enumeration {}: {error}",
                        parent.display()
                    ),
                )
            })?;
        let duplicate = unsafe { libc::dup(readable_fd.as_raw_fd()) };
        if duplicate < 0 {
            let error = std::io::Error::last_os_error();
            return Err(std::io::Error::new(
                error.kind(),
                format!(
                    "failed to duplicate directory fd for {}: {}",
                    parent.display(),
                    error
                ),
            ));
        }
        let directory = unsafe { libc::fdopendir(duplicate) };
        if directory.is_null() {
            let error = std::io::Error::last_os_error();
            unsafe { libc::close(duplicate) };
            return Err(std::io::Error::new(
                error.kind(),
                format!(
                    "failed to enumerate directory {}: {error}",
                    parent.display()
                ),
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
                    return Err(std::io::Error::new(
                        error.kind(),
                        format!("failed while enumerating {}: {error}", parent.display()),
                    ));
                }
                let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
                if name == b"." || name == b".." {
                    continue;
                }
                let name = OsStr::from_bytes(name);
                let diagnostic_path = parent.join(name);
                let diagnostic = match std::fs::symlink_metadata(&diagnostic_path) {
                    Ok(metadata) => metadata,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => {
                        return Err(std::io::Error::new(
                            error.kind(),
                            format!(
                                "directory entry changed while inspecting {}: {error}",
                                diagnostic_path.display()
                            ),
                        ))
                    }
                };
                if diagnostic.file_type().is_symlink() {
                    continue;
                }

                let child_fd = match open_child_no_symlinks(parent_fd.as_raw_fd(), name) {
                    Ok(fd) => fd,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => {
                        return Err(std::io::Error::new(
                            error.kind(),
                            format!(
                                "directory entry changed while opening {}: {error}",
                                diagnostic_path.display()
                            ),
                        ))
                    }
                };
                let metadata = fstat_fd(&child_fd).map_err(|error| {
                    std::io::Error::new(
                        error.kind(),
                        format!(
                            "failed to inspect opened directory entry {}: {error}",
                            diagnostic_path.display()
                        ),
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
fn open_absolute_no_symlinks(path: &Path, directory: bool) -> std::io::Result<OwnedFd> {
    if !path.is_absolute() {
        return Err(std::io::Error::other(format!(
            "path is not absolute: {}",
            path.display()
        )));
    }
    let root = unsafe {
        libc::open(
            c"/".as_ptr(),
            libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if root < 0 {
        let error = std::io::Error::last_os_error();
        return Err(std::io::Error::new(
            error.kind(),
            format!("failed to open filesystem root: {}", error),
        ));
    }
    let root = unsafe { OwnedFd::from_raw_fd(root) };
    let components = normalized_relative_components(path).map_err(std::io::Error::other)?;
    if components.is_empty() {
        return Ok(root);
    }

    let relative = components
        .iter()
        .fold(PathBuf::new(), |path, component| path.join(component));
    let relative = CString::new(relative.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::other(format!("path contains NUL: {}", path.display())))?;
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
        return Err(std::io::Error::new(
            error.kind(),
            format!("secure open failed for {}: {error}", path.display()),
        ));
    }

    let mut current = root;
    for (index, component) in components.iter().enumerate() {
        let component = CString::new(component.as_bytes())
            .map_err(|_| std::io::Error::other(format!("path contains NUL: {}", path.display())))?;
        let last = index + 1 == components.len();
        let mut flags = libc::O_PATH | libc::O_CLOEXEC | libc::O_NOFOLLOW;
        if !last || directory {
            flags |= libc::O_DIRECTORY;
        }
        let opened = unsafe { libc::openat(current.as_raw_fd(), component.as_ptr(), flags) };
        if opened < 0 {
            let error = std::io::Error::last_os_error();
            return Err(std::io::Error::new(
                error.kind(),
                format!(
                    "component-wise secure open failed for {}: {}",
                    path.display(),
                    error
                ),
            ));
        }
        let opened = unsafe { OwnedFd::from_raw_fd(opened) };
        let metadata = fstat_fd(&opened).map_err(|error| {
            std::io::Error::new(
                error.kind(),
                format!("failed to inspect {}: {error}", path.display()),
            )
        })?;
        if metadata.st_mode & libc::S_IFMT == libc::S_IFLNK {
            return Err(std::io::Error::other(format!(
                "secure open encountered a symlink: {}",
                path.display()
            )));
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
pub(crate) fn create_task_temp_dir(task_bundle_dir: &Path) -> Result<PathBuf, String> {
    task_temp::create(task_bundle_dir)
        .map_err(|error| format!("failed to allocate task temp directory: {error}"))
}

/// Host-owned namespace for command-private scratch and broker endpoints.
#[cfg(unix)]
pub fn managed_task_temp_root() -> &'static Path {
    task_temp::root()
}

#[cfg(target_os = "macos")]
fn install_worker_mktemp(temp_dir: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let bin = temp_dir.join(".worker-bin");
    std::fs::create_dir(&bin)
        .map_err(|error| format!("failed to create task temporary helpers: {error}"))?;
    let shim = bin.join("mktemp");
    // Apple mktemp prefers confstr(_CS_DARWIN_USER_TEMP_DIR) over TMPDIR
    // when no template is supplied. An explicit template keeps that default
    // operation inside the task; user-supplied templates retain their meaning.
    use std::io::Write;
    static NEXT_TEMPORARY_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let temporary = bin.join(format!(
        ".mktemp.{}.{}",
        std::process::id(),
        NEXT_TEMPORARY_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let mut staged = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|error| format!("failed to stage task temporary helper: {error}"))?;
    let written = (|| {
        staged.write_all(
            br#"#!/bin/sh
for argument do
    case "$argument" in
        -*) flags=${argument#-}
            case "$flags" in ''|*[!dqu]*) exec /usr/bin/mktemp "$@" ;; esac ;;
        *) exec /usr/bin/mktemp "$@" ;;
    esac
done
exec /usr/bin/mktemp "$@" "${TMPDIR:?}/tmp.XXXXXXXX"
"#,
        )?;
        staged.set_permissions(std::fs::Permissions::from_mode(0o755))
    })();
    drop(staged);
    if let Err(error) = written {
        let _ = std::fs::remove_file(&temporary);
        return Err(format!("failed to write task temporary helper: {error}"));
    }
    // A child must never execute this helper while another process still has
    // its inode open for writing; Linux reports ETXTBSY (text file busy) in that situation.
    std::fs::rename(&temporary, &shim).map_err(|error| {
        let _ = std::fs::remove_file(&temporary);
        format!("failed to publish task temporary helper: {error}")
    })
}

pub(crate) fn is_managed_task_temp_dir(path: &Path) -> bool {
    #[cfg(unix)]
    if task_temp::is_managed(path) {
        return true;
    }
    path.file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| name.starts_with(".sandbox-tmp-"))
}

pub(crate) fn cleanup_task_temp_dir(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    if task_temp::is_managed(path) {
        return task_temp::remove(path);
    }
    // Older persisted tasks kept their temporary files inside the IO bundle.
    if is_managed_task_temp_dir(path) {
        return match std::fs::remove_dir_all(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            result => result,
        };
    }
    Ok(())
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

#[cfg(unix)]
fn native_launcher_path(ctx: &AppContext) -> std::io::Result<PathBuf> {
    #[cfg(any(test, feature = "test-sandbox-launcher"))]
    if let Some(launchers) = TEST_LAUNCHERS.get() {
        if let Some(root) = ctx.config().project_root.as_deref() {
            if let Some(path) = launchers
                .lock()
                .expect("sandbox launcher test mutex poisoned")
                .get(&observation_key(root))
            {
                return Ok(path.clone());
            }
        }
    }
    let _ = ctx;
    std::env::current_exe()
}

/// Use the real aft executable for an in-process integration fixture. The test
/// harness cannot re-exec itself as `aft sandbox-launch`; it only accepts libtest
/// arguments. Only executable selection changes, not the native sandbox policy.
#[cfg(all(unix, any(test, feature = "test-sandbox-launcher")))]
#[doc(hidden)]
pub fn install_sandbox_launcher_for_test(project_root: &Path, launcher_path: PathBuf) -> impl Drop {
    struct LauncherScope(PathBuf, Option<PathBuf>);

    impl Drop for LauncherScope {
        fn drop(&mut self) {
            let mut launchers = TEST_LAUNCHERS
                .get()
                .expect("sandbox launcher test map installed")
                .lock()
                .expect("sandbox launcher test mutex poisoned");
            if let Some(previous) = self.1.take() {
                launchers.insert(self.0.clone(), previous);
            } else {
                launchers.remove(&self.0);
            }
        }
    }

    let root = observation_key(project_root);
    let previous = TEST_LAUNCHERS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .expect("sandbox launcher test mutex poisoned")
        .insert(root.clone(), launcher_path);
    LauncherScope(root, previous)
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
pub(crate) fn apply_marker_fd_allowlist(
    command: &mut Command,
    exit_fd: RawFd,
    failure_fd: RawFd,
) -> Result<(RawFd, RawFd), String> {
    use std::os::unix::process::CommandExt;

    let open_fds = open_descriptor_snapshot().map_err(|error| error.to_string())?;
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
            if libc::dup2(exit_copy, CHILD_EXIT_FD) < 0
                || libc::dup2(failure_copy, CHILD_FAILURE_FD) < 0
            {
                let error = std::io::Error::last_os_error();
                libc::close(exit_copy);
                libc::close(failure_copy);
                return Err(error);
            }
            libc::close(exit_copy);
            libc::close(failure_copy);
            mark_extra_fds_cloexec(&open_fds)
        });
    }
    Ok((CHILD_EXIT_FD, CHILD_FAILURE_FD))
}

#[cfg(unix)]
fn open_descriptor_snapshot() -> std::io::Result<Vec<RawFd>> {
    // Enumerate and allocate before fork: the child only visits actual descriptors,
    // not every unused number below RLIMIT_NOFILE. Descriptors opened by other
    // threads or spawn machinery after this snapshot must retain atomic CLOEXEC;
    // pre_exec hooks must not create additional inheritable descriptors. Only the
    // two explicitly remapped markers are exempt from that invariant.
    let mut fds = list_open_descriptors()?;
    fds.retain(|fd| *fd > CHILD_FAILURE_FD);
    fds.sort_unstable();
    fds.dedup();
    Ok(fds)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn list_open_descriptors() -> std::io::Result<Vec<RawFd>> {
    let directory = if cfg!(target_os = "linux") {
        "/proc/self/fd"
    } else {
        "/dev/fd"
    };
    let mut fds = Vec::new();
    for entry in std::fs::read_dir(directory)? {
        if let Some(fd) = entry?
            .file_name()
            .to_str()
            .and_then(|name| name.parse().ok())
        {
            fds.push(fd);
        }
    }
    Ok(fds)
}

#[cfg(target_os = "macos")]
fn list_open_descriptors() -> std::io::Result<Vec<RawFd>> {
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct ProcFdInfo {
        fd: i32,
        fd_type: u32,
    }
    #[link(name = "proc")]
    unsafe extern "C" {
        fn proc_pidinfo(
            pid: i32,
            flavor: i32,
            arg: u64,
            buffer: *mut libc::c_void,
            size: i32,
        ) -> i32;
    }
    const PROC_PIDLISTFDS: i32 = 1;
    let pid = unsafe { libc::getpid() };
    let size = unsafe { proc_pidinfo(pid, PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
    if size <= 0 {
        return Err(std::io::Error::last_os_error());
    }
    let record_size = std::mem::size_of::<ProcFdInfo>();
    let mut capacity = size as usize / record_size + 32;
    loop {
        let byte_size = capacity
            .checked_mul(record_size)
            .and_then(|bytes| i32::try_from(bytes).ok())
            .ok_or_else(|| std::io::Error::other("descriptor inventory is too large"))?;
        let mut records = vec![ProcFdInfo { fd: 0, fd_type: 0 }; capacity];
        let written = unsafe {
            proc_pidinfo(
                pid,
                PROC_PIDLISTFDS,
                0,
                records.as_mut_ptr().cast(),
                byte_size,
            )
        };
        if written <= 0 {
            return Err(std::io::Error::last_os_error());
        }
        if written >= byte_size {
            // A full buffer may omit concurrent opens. Retry with room to grow
            // rather than silently dropping the highest descriptor entries.
            capacity = capacity
                .checked_mul(2)
                .ok_or_else(|| std::io::Error::other("descriptor inventory capacity overflow"))?;
            continue;
        }
        if written as usize % record_size != 0 {
            return Err(std::io::Error::other("partial descriptor inventory record"));
        }
        return Ok(records[..written as usize / record_size]
            .iter()
            .map(|record| record.fd)
            .collect());
    }
}

#[cfg(unix)]
fn mark_extra_fds_cloexec(open_fds: &[RawFd]) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    // CLOEXEC leaves Rust's spawn-error pipe usable until exec succeeds and
    // preserves the two marker descriptors below the range.
    if unsafe {
        libc::syscall(
            libc::SYS_close_range,
            (CHILD_FAILURE_FD + 1) as u32,
            u32::MAX,
            libc::CLOSE_RANGE_CLOEXEC,
        )
    } == 0
    {
        return Ok(());
    }
    mark_extra_fds_cloexec_fallback(open_fds)
}

#[cfg(unix)]
fn mark_extra_fds_cloexec_fallback(open_fds: &[RawFd]) -> std::io::Result<()> {
    mark_extra_fds_cloexec_fallback_with(open_fds, |fd, operation, flags| unsafe {
        libc::fcntl(fd, operation, flags)
    })
}

#[cfg(unix)]
fn mark_extra_fds_cloexec_fallback_with(
    fds: &[RawFd],
    mut fcntl: impl FnMut(RawFd, libc::c_int, libc::c_int) -> libc::c_int,
) -> std::io::Result<()> {
    for &fd in fds {
        let flags = fcntl(fd, libc::F_GETFD, 0);
        if flags < 0 {
            let error = std::io::Error::last_os_error();
            // The listing handle or another thread's descriptor may have closed
            // since the parent snapshot. No descriptor remains to protect then.
            if error.raw_os_error() == Some(libc::EBADF) {
                continue;
            }
            return Err(error);
        }
        if flags & libc::FD_CLOEXEC == 0 && fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
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
    // Session setup requires pre_exec, so execvp can resolve the final child PATH.
    let mut command = crate::effective_path::new_command(program).build_unresolved();
    command.args(args);
    Ok((command, profile_handle))
}

#[cfg(unix)]
pub(crate) fn spawn_detached_for_plan(
    plan: &SpawnPlan,
    command: &mut Command,
    environment: &HashMap<String, String>,
    io: &crate::posix_spawn::PipeIo<'_>,
) -> std::io::Result<crate::posix_spawn::Child> {
    command.env_remove(crate::privacy_spawn::CONTROL_ENV);
    if let Some(result) = crate::posix_spawn::try_spawn(
        command,
        isolated_environment_for_plan(plan, environment).is_some(),
        crate::privacy_spawn::requested(environment),
        io,
    ) {
        return result;
    }
    // Old libcs cannot express the session/fd contract through file actions.
    // Keep their existing fork path rather than weaken isolation or break bash.
    crate::bash_background::process::start_new_session(command);
    apply_marker_fd_allowlist(command, io.exit, io.failure).map_err(std::io::Error::other)?;
    command
        .stdin(if io.gated_stdin {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .stdout(io.stdout.try_clone()?)
        .stderr(io.stderr.try_clone()?);
    disclaim_command_for_plan(plan, command, environment).map_err(std::io::Error::other)?;
    command.spawn().map(Into::into)
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
        SpawnPlan::Launcher { profile, .. } => {
            let mut environment =
                sandboxed_child_environment(request_environment, &profile.temp_dir);
            #[cfg(target_os = "macos")]
            if profile.temp_dir.join(".worker-bin/mktemp").is_file() {
                let mut paths = vec![profile.temp_dir.join(".worker-bin")];
                if let Some(path) = environment.get(OsStr::new("PATH")) {
                    paths.extend(std::env::split_paths(path));
                }
                if let Ok(path) = std::env::join_paths(paths) {
                    environment.insert(OsString::from("PATH"), path);
                }
            }
            // RUSTC_WRAPPER was emptied to disable out-of-sandbox compile servers.
            // Restore only the direct launcher whose executable this profile
            // resolved from operator config and explicitly made read-only.
            if let Some(wrapper) = profile.data_policy.read_allow.iter().find(|path| {
                path.file_name() == Some(OsStr::new("prefrontal-build-slot"))
                    && profile.write_deny.contains(path)
            }) {
                for key in ["RUSTC_WRAPPER", "CARGO_BUILD_RUSTC_WRAPPER"] {
                    environment.insert(OsString::from(key), wrapper.as_os_str().to_os_string());
                }
            }
            Some(environment)
        }
        SpawnPlan::RelaxedUnenforced => Some(sandboxed_child_environment(
            request_environment,
            &std::env::temp_dir(),
        )),
        SpawnPlan::Unsandboxed | SpawnPlan::Refused { .. } => None,
        #[cfg(unix)]
        SpawnPlan::Prepared { .. } => unreachable!("policy() unwraps prepared plans"),
    }
}

/// Stores live output secrets and whether any non-ticket secret must remain
/// withheld after restart. The ticket digest lets previews redact that exact value.
pub(crate) struct CompletionSecrets {
    pub(crate) values: crate::log_redact::SecretValues,
    pub(crate) has_non_ticket_secrets: bool,
    pub(crate) ticket_digest: Option<String>,
}

/// Snapshot the environment selected for the child, not the daemon's ambient
/// credentials. Unsandboxed children intentionally inherit the parent's environment.
pub(crate) fn completion_secrets(
    plan: &SpawnPlan,
    request_environment: &HashMap<String, String>,
) -> CompletionSecrets {
    let mut environment =
        isolated_environment_for_plan(plan, request_environment).unwrap_or_else(|| {
            let mut environment: ChildEnvironment = std::env::vars_os().collect();
            #[cfg(test)]
            for key in crate::test_env::secret_named_environment_keys(
                crate::log_redact::is_secret_environment_key,
            ) {
                environment.remove(&key);
            }
            environment.extend(
                request_environment
                    .iter()
                    .map(|(key, value)| (key.into(), value.into())),
            );
            environment
        });
    environment.retain(|key, _| {
        !key.to_str().is_some_and(|key| {
            crate::agent_child_env::is_subc_credential_env_key(key)
                || key == crate::privacy_spawn::CONTROL_ENV
        })
    });
    let has_non_ticket_secrets = !crate::log_redact::SecretValues::from_environment(
        environment
            .iter()
            .filter(|(key, _)| {
                !key.to_str().is_some_and(|key| {
                    key.eq_ignore_ascii_case(crate::gh_shim_ticket::GH_SHIM_TICKET_ENV)
                })
            })
            .map(|(key, value)| (key.as_os_str(), value.as_os_str())),
    )
    .is_empty();
    environment.retain(|key, _| {
        !key.to_str()
            .is_some_and(|key| key.eq_ignore_ascii_case(crate::gh_shim_ticket::GH_SHIM_TICKET_ENV))
    });
    let ticket = memory_only_ticket(request_environment);
    let ticket_digest = ticket.map(|ticket| blake3::hash(ticket.as_bytes()).to_hex().to_string());
    if let Some(ticket) = ticket {
        environment.insert(
            crate::gh_shim_ticket::GH_SHIM_TICKET_ENV.into(),
            ticket.into(),
        );
    }
    CompletionSecrets {
        values: crate::log_redact::SecretValues::from_environment(
            environment
                .iter()
                .map(|(key, value)| (key.as_os_str(), value.as_os_str())),
        ),
        has_non_ticket_secrets,
        ticket_digest,
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
        // Workers also drop session-bus discovery; Linux still reports that
        // direct connections to a known bus path are not isolated by Landlock.
        if crate::bash_env::is_preexec_hijack_env_key(key.as_str())
            || key == crate::privacy_spawn::CONTROL_ENV
            || crate::agent_child_env::is_subc_credential_env_key(key)
            || key == crate::gh_shim_ticket::GH_SHIM_TICKET_ENV
            || key == crate::gh_shim_worker_broker::SOCKET_ENV
            || key == crate::gh_shim_worker_broker::CAPABILITY_FILE_ENV
            || (worker_jail_active() && key == "DBUS_SESSION_BUS_ADDRESS")
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
        #[cfg(windows)]
        let key = key.to_ascii_uppercase();
        #[cfg(windows)]
        let key = key.as_str();
        #[cfg(windows)]
        if matches!(
            key,
            "SYSTEMROOT"
                | "WINDIR"
                | "COMSPEC"
                | "PATHEXT"
                | "USERPROFILE"
                | "HOMEDRIVE"
                | "HOMEPATH"
                | "APPDATA"
                | "LOCALAPPDATA"
        ) {
            return true;
        }
        matches!(key, "HOME" | "USER" | "LOGNAME" | "SHELL" | "TERM" | "LANG")
            || key.starts_with("LC_")
    })
}

#[cfg(unix)]
pub(crate) fn approved_environment_for_plan(
    plan: &SpawnPlan,
    request_environment: &HashMap<String, String>,
) -> ChildEnvironment {
    isolated_environment_for_plan(plan, request_environment)
        .unwrap_or_else(|| approved_payload_environment(request_environment, &std::env::temp_dir()))
}

pub(crate) fn apply_sandbox_environment(
    plan: &SpawnPlan,
    command: &mut Command,
    request_environment: &HashMap<String, String>,
) {
    if let Some(environment) = isolated_environment_for_plan(plan, request_environment) {
        // Isolated plans supply complete child environments even when file
        // restrictions are unenforced. Clear the daemon environment so loader hooks,
        // shell startup hooks, and cloud credentials cannot leak around them.
        command.env_clear().envs(environment);
        if let Some(ticket) = memory_only_ticket(request_environment) {
            command.env(crate::gh_shim_ticket::GH_SHIM_TICKET_ENV, ticket);
        }
        for key in [
            crate::gh_shim_worker_broker::SOCKET_ENV,
            crate::gh_shim_worker_broker::CAPABILITY_FILE_ENV,
        ] {
            if let Some(value) = request_environment.get(key) {
                command.env(key, value);
            }
        }
    }
}

/// Install after all environment/session/fd adapters and before spawning.
#[cfg(unix)]
pub(crate) fn disclaim_command_for_plan(
    plan: &SpawnPlan,
    command: &mut Command,
    environment: &HashMap<String, String>,
) -> Result<(), String> {
    command.env_remove(crate::privacy_spawn::CONTROL_ENV);
    crate::privacy_spawn::install(
        command,
        crate::privacy_spawn::requested(environment),
        isolated_environment_for_plan(plan, environment).is_some(),
    )
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
    #[cfg(windows)]
    {
        // portable-pty refreshes its base environment from the registry, which
        // can replace a process-local PATH and hide the shell we just selected.
        // Inherit the actual parent block, as std::process::Command does, before
        // applying the launch plan's isolation and request overrides below.
        command.env_clear();
        for (key, value) in std::env::vars_os() {
            command.env(key, value);
        }
    }
    for arg in args {
        command.arg(arg);
    }
    crate::agent_child_env::scrub_pty_command(&mut command);
    command.cwd(workdir.as_os_str());
    if let Some(environment) = isolated_environment_for_plan(plan, env) {
        command.env_clear();
        for (key, value) in environment {
            command.env(key, value);
        }
        if let Some(ticket) = memory_only_ticket(env) {
            command.env(crate::gh_shim_ticket::GH_SHIM_TICKET_ENV, ticket);
        }
        for key in [
            crate::gh_shim_worker_broker::SOCKET_ENV,
            crate::gh_shim_worker_broker::CAPABILITY_FILE_ENV,
        ] {
            if let Some(value) = env.get(key) {
                command.env(key, value);
            }
        }
    } else {
        // Sandbox-disabled PTYs retain the historical full inheritance and add
        // only request overrides. An inherited gh shim ticket is removed first:
        // it belongs to whatever started this process, and the request map
        // re-adds the child's own ticket when one was issued.
        command.env_remove(crate::gh_shim_ticket::GH_SHIM_TICKET_ENV);
        command.env_remove(crate::gh_shim_ticket::GH_SHIM_WORKER_ENV);
        for (key, value) in env {
            command.env(key, value);
        }
    }
    // Scrub credentials after explicit overrides too; only the command's own
    // governance paths and ticket may be restored from the trusted spawn map.
    crate::agent_child_env::scrub_pty_command(&mut command);
    for key in crate::gh_shim_worker_broker::CHILD_KEYS {
        if let Some(value) = env.get(key) {
            command.env(key, value);
        }
    }
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
        SpawnPlan::Unsandboxed | SpawnPlan::RelaxedUnenforced | SpawnPlan::Host { .. } => {
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

/// A request's resolved shell policy for project-controlled formatters and
/// checkers. The owner keeps the private task directory alive across all of
/// the request's tools, then removes it even on refusal or an early return.
pub struct ToolLaunchPolicy {
    plan: SpawnPlan,
    environment: HashMap<String, String>,
    task: Option<crate::bash_background::persistence::ResolvedTask>,
}

thread_local! {
    static CURRENT_TOOL_POLICY: RefCell<Option<std::rc::Rc<ToolLaunchPolicy>>> = const { RefCell::new(None) };
}

impl Drop for ToolLaunchPolicy {
    fn drop(&mut self) {
        self.plan.cleanup_unspawned();
        if let Some(task) = &self.task {
            let _ = crate::bash_background::persistence::delete_resolved_task(task);
        }
    }
}

impl ToolLaunchPolicy {
    /// Direct tools carry only their formatter policy across dispatch. General
    /// sandbox scopes must not cover dispatch: it can start shared LSP servers.
    pub(crate) fn with_current<R>(policy: Option<std::rc::Rc<Self>>, run: impl FnOnce() -> R) -> R {
        struct Restore(Option<std::rc::Rc<ToolLaunchPolicy>>);
        impl Drop for Restore {
            fn drop(&mut self) {
                CURRENT_TOOL_POLICY.with(|slot| {
                    slot.replace(self.0.take());
                });
            }
        }
        let previous = CURRENT_TOOL_POLICY.with(|slot| slot.replace(policy));
        let _restore = Restore(previous);
        run()
    }

    pub(crate) fn for_request(ctx: &AppContext) -> std::rc::Rc<Self> {
        CURRENT_TOOL_POLICY
            .with(|slot| slot.borrow().clone())
            .unwrap_or_else(|| std::rc::Rc::new(Self::resolve(ctx)))
    }

    pub(crate) fn for_session(
        ctx: &AppContext,
        principal: AuthenticatedPrincipal,
        worker: bool,
    ) -> std::rc::Rc<Self> {
        std::rc::Rc::new(with_authenticated_principal(principal, || {
            with_worker_jail(worker, || Self::resolve(ctx))
        }))
    }

    pub(crate) fn resolve(ctx: &AppContext) -> Self {
        let principal = current_authenticated_principal();
        let mode = effective_sandbox_mode(ctx, &principal);
        let mut environment = HashMap::new();
        apply_worker_jail_environment(&mut environment, mode);
        let mut policy = Self {
            plan: SpawnPlan::Unsandboxed,
            environment,
            task: None,
        };
        let needs_layout = cfg!(unix) && (mode != SandboxMode::Off || worker_jail_active());
        if needs_layout {
            let session = crate::log_ctx::current_session().unwrap_or_else(|| "aft-tools".into());
            match crate::bash_background::persistence::allocate_task_layout(
                &ctx.storage_dir(),
                &session,
            ) {
                Ok(task) => policy.task = Some(task),
                Err(error) => {
                    policy.plan = SpawnPlan::Refused {
                        code: "sandbox_unavailable",
                        message: format!("failed to allocate a private tool directory: {error}"),
                        mismatch_class: None,
                    };
                    return policy;
                }
            }
        }
        let io_dir = policy
            .task
            .as_ref()
            .map(|task| task.paths.io_dir.as_path())
            .unwrap_or(Path::new(""));
        policy.plan = resolve_sandbox_spawn(
            ctx,
            &principal,
            RequestedSandboxTier::Native,
            SandboxTaskKind::ExternalTool,
            io_dir,
            None,
        );
        policy
    }

    pub(crate) fn command(&self, program: &OsStr, args: &[OsString]) -> Result<Command, String> {
        self.command_with_base_environment(program, args, false)
    }

    /// Bash syntax checks must not read daemon-provided shell startup hooks.
    /// Clear inherited state before installing any policy-required environment.
    pub(crate) fn command_with_clean_environment(
        &self,
        program: &OsStr,
        args: &[OsString],
    ) -> Result<Command, String> {
        self.command_with_base_environment(program, args, true)
    }

    fn command_with_base_environment(
        &self,
        program: &OsStr,
        args: &[OsString],
        clean: bool,
    ) -> Result<Command, String> {
        // Host grants bind one approved shell command, never an edit's tools.
        if matches!(self.plan.policy(), SpawnPlan::Host { .. }) {
            return Err(
                "sandbox_unavailable: host escalation cannot run a formatter or checker".into(),
            );
        }
        if let SpawnPlan::Refused { code, message, .. } = self.plan.policy() {
            return Err(format!("{code}: {message}"));
        }
        let (program, args, _profile_handle) =
            command_argv_for_plan(&self.plan, program, args, Path::new(""), None)?;
        #[cfg(windows)]
        let mut command = if crate::windows_command::is_batch_file(Path::new(&program)) {
            crate::windows_command::batch_command(Path::new(&program), &args)
                .map_err(|error| error.to_string())?
        } else {
            let mut command = crate::effective_path::new_command(program).build();
            command.args(args);
            command
        };
        #[cfg(not(windows))]
        let mut command = {
            let mut command = crate::effective_path::new_command(program).build();
            command.args(args);
            command
        };
        if clean {
            command.env_clear();
            if let Some(path) = std::env::var_os("PATH") {
                command.env("PATH", path);
            }
        }
        apply_sandbox_environment(&self.plan, &mut command, &self.environment);
        #[cfg(unix)]
        if !matches!(self.plan.policy(), SpawnPlan::Unsandboxed) {
            disclaim_command_for_plan(&self.plan, &mut command, &self.environment)?;
        }
        Ok(command)
    }

    #[cfg(test)]
    pub(crate) fn unconfined_for_test() -> Self {
        Self {
            plan: SpawnPlan::Unsandboxed,
            environment: HashMap::new(),
            task: None,
        }
    }
}

/// Auxiliary read-only CLI lookups inherit the command's exact launch policy,
/// rather than resolving a new principal or silently falling back to the host.
#[cfg(unix)]
pub(crate) fn probe_command_for_plan(
    plan: &SpawnPlan,
    program: &OsStr,
    args: &[OsString],
    cwd: &Path,
    env: &HashMap<String, String>,
) -> Result<Command, String> {
    let (program, args, _profile_handle) =
        command_argv_for_plan(plan, program, args, Path::new(""), None)?;
    // Session setup requires pre_exec, so execvp can resolve the final child PATH.
    let mut command = crate::effective_path::new_command(program).build_unresolved();
    command.args(args).current_dir(cwd);
    crate::agent_child_env::apply_to_command(&mut command, env);
    apply_sandbox_environment(plan, &mut command, env);
    crate::bash_background::process::start_new_session(&mut command);
    disclaim_command_for_plan(plan, &mut command, env)?;
    Ok(command)
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
    // With marker descriptors, a `/bin/sh` supervisor records an exit status
    // when the launcher fails before the target runs. Exit status 78 alone
    // cannot say who failed, because the target may legitimately exit 78
    // after the launcher has exec'd it. The launcher therefore writes the
    // `sandbox_unavailable` failure marker itself, through `--failure-fd`,
    // only when confinement could not be applied; the supervisor reads that
    // marker instead of guessing from the status.
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
"$launcher" sandbox-launch --profile-json "$profile_json" --failure-fd "$failure_fd" -- "$@"
code=$?
if [ "$code" -eq 78 ] && [ -s "/dev/fd/$failure_fd" ]; then
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

#[cfg(all(test, windows))]
mod windows_pty_environment_tests {
    use super::*;

    #[test]
    fn windows_pty_inherits_process_search_path_not_registry_environment() {
        environment_tests::with_daemon_environment(
            "sandbox_spawn::windows_pty_environment_tests::windows_pty_inherits_process_search_path_not_registry_environment",
            || {
                let dir = tempfile::tempdir().unwrap();
                let (command, _) = pty_command_for_plan(
                    &SpawnPlan::Unsandboxed,
                    OsStr::new("pwsh.exe"),
                    &[],
                    &dir.path().join("metadata.json"),
                    dir.path(),
                    &HashMap::new(),
                )
                .unwrap();
                for key in ["PATH", "PATHEXT", "TEMP", "USERPROFILE"] {
                    assert_eq!(
                        command.get_env(key),
                        std::env::var_os(key).as_deref(),
                        "{key}"
                    );
                }
                assert_eq!(command.get_env("SUBC_TEST_CREDENTIAL"), None);
            },
        );
    }

    #[test]
    fn windows_pty_request_environment_overrides_process_environment() {
        let dir = tempfile::tempdir().unwrap();
        let request = HashMap::from([
            ("Path".to_string(), dir.path().display().to_string()),
            ("PATHEXT".to_string(), ".AFT".to_string()),
        ]);
        let (command, _) = pty_command_for_plan(
            &SpawnPlan::Unsandboxed,
            OsStr::new("pwsh.exe"),
            &[],
            &dir.path().join("metadata.json"),
            dir.path(),
            &request,
        )
        .unwrap();
        assert_eq!(command.get_env("PATH"), Some(OsStr::new(&request["Path"])));
        assert_eq!(command.get_env("PATHEXT"), Some(OsStr::new(".AFT")));
    }
}

#[cfg(test)]
pub(crate) mod environment_tests {
    use super::*;

    #[test]
    #[cfg(unix)]
    fn worker_keychain_policy_only_marks_worker_profiles() {
        let dir = tempfile::tempdir().unwrap();
        let mut profile = SandboxProfile::build(
            vec![dir.path().to_path_buf()],
            vec![],
            vec![],
            vec![],
            vec![],
            vec![],
            vec![],
            dir.path().to_path_buf(),
        )
        .unwrap();
        protect_worker_credential_services(&mut profile, dir.path()).unwrap();
        assert!(!profile.worker_jail);
        assert!(profile.keychain_deny.is_empty());
        with_worker_jail(true, || {
            protect_worker_credential_services(&mut profile, dir.path())
        })
        .unwrap();
        assert!(profile.worker_jail);
        #[cfg(target_os = "macos")]
        {
            assert!(profile
                .keychain_deny
                .contains(&dir.path().join("Library/Keychains")));
            assert!(profile
                .keychain_deny
                .contains(&macos_account_home().unwrap().join("Library/Keychains")));
            assert!(profile
                .keychain_deny
                .contains(&PathBuf::from("/Library/Keychains")));
        }
        let profile = profile.canonicalize_for_launch().unwrap();
        let decoded: SandboxProfile =
            serde_json::from_slice(&serde_json::to_vec(&profile).unwrap()).unwrap();
        assert!(decoded.worker_jail);
        assert_eq!(decoded.keychain_deny, profile.keychain_deny);
    }

    #[test]
    #[cfg(unix)]
    fn worker_session_bus_address_is_stripped_without_changing_heads() {
        let request = HashMap::from([(
            "DBUS_SESSION_BUS_ADDRESS".to_string(),
            "unix:path=/run/user/1000/bus".to_string(),
        )]);
        let dir = tempfile::tempdir().unwrap();
        let head = with_worker_jail(false, || sandboxed_child_environment(&request, dir.path()));
        assert_eq!(
            head.get(OsStr::new("DBUS_SESSION_BUS_ADDRESS")),
            Some(&OsString::from("unix:path=/run/user/1000/bus"))
        );
        let worker = with_worker_jail(true, || sandboxed_child_environment(&request, dir.path()));
        assert!(!worker.contains_key(OsStr::new("DBUS_SESSION_BUS_ADDRESS")));
        assert!(!sandbox_base_environment_key(OsStr::new(
            "DBUS_SESSION_BUS_ADDRESS"
        )));
    }

    #[test]
    #[cfg(unix)]
    fn worker_environment_keeps_benign_values_and_strips_injection_and_credentials() {
        let request = HashMap::from([
            ("BUILD_LABEL".into(), "configured-value".into()),
            ("BASH_ENV".into(), "/untrusted/startup".into()),
            (
                "DBUS_SESSION_BUS_ADDRESS".into(),
                "unix:path=/run/user/1000/bus".into(),
            ),
            (
                crate::privacy_spawn::CONTROL_ENV.into(),
                "private-control".into(),
            ),
            (
                crate::gh_shim_ticket::GH_SHIM_TICKET_ENV.into(),
                "private-ticket".into(),
            ),
        ]);
        let dir = tempfile::tempdir().unwrap();
        let worker = with_worker_jail(true, || sandboxed_child_environment(&request, dir.path()));
        assert_eq!(
            worker.get(OsStr::new("BUILD_LABEL")),
            Some(&OsString::from("configured-value"))
        );
        for key in request.keys().filter(|key| key.as_str() != "BUILD_LABEL") {
            assert!(
                !worker.contains_key(OsStr::new(key)),
                "worker inherited {key}"
            );
        }
    }

    const REENTRY: &str = "AFT_TEST_RELAXED_ENV_REENTRY";

    // Plant daemon variables in a subprocess, not the parallel test runner's
    // process-wide environment. Both adapters then see genuine inherited data.
    pub(crate) fn with_daemon_environment(test: &str, check: impl FnOnce()) {
        if std::env::var(REENTRY).as_deref() == Ok(test) {
            assert_eq!(
                std::env::var("AWS_SECRET_ACCESS_KEY").unwrap(),
                "daemon-secret"
            );
            check();
            return;
        }
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", test, "--nocapture"])
            .env(REENTRY, test)
            .env("AWS_SECRET_ACCESS_KEY", "daemon-secret")
            .env("SUBC_TEST_CREDENTIAL", "daemon-subc-secret")
            .env("TERM", "aft-inherited-term");
        #[cfg(windows)]
        let process_bin = tempfile::tempdir().unwrap();
        #[cfg(windows)]
        {
            // A process-only directory makes registry-based PATH inheritance
            // observably different without changing the test runner's environment.
            let mut paths = vec![process_bin.path().to_path_buf()];
            paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
            command.env("PATH", std::env::join_paths(paths).unwrap());
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "environment regression subprocess failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    pub(crate) fn request_environment() -> HashMap<String, String> {
        HashMap::from([
            ("REQUEST_SENTINEL".to_string(), "request-value".to_string()),
            ("GIT_CONFIG_COUNT".to_string(), "1".to_string()),
            ("GIT_CONFIG_KEY_0".to_string(), "core.autocrlf".to_string()),
            ("GIT_CONFIG_VALUE_0".to_string(), "false".to_string()),
        ])
    }

    pub(crate) fn assert_stripped_environment(
        environment: HashMap<String, String>,
        through_shell: bool,
    ) {
        assert!(
            !environment.contains_key("AWS_SECRET_ACCESS_KEY"),
            "daemon secret leaked into relaxed-unenforced child"
        );
        assert!(
            !environment.contains_key("SUBC_TEST_CREDENTIAL"),
            "daemon subc credential leaked into child"
        );
        let path = environment.get("PATH").expect("child PATH");
        let expected_path = crate::effective_path::effective_path().to_str().unwrap();
        // PowerShell prepends its installation directory to PATH and may append
        // extensions to PATHEXT. Direct adapter checks still require exact values.
        assert!(
            path == expected_path
                || (through_shell && path.ends_with(&format!(";{expected_path}"))),
            "child lost AFT's effective PATH"
        );
        assert_eq!(
            environment.get("TERM").map(String::as_str),
            Some("aft-inherited-term")
        );
        for (key, value) in request_environment() {
            assert_eq!(environment.get(&key), Some(&value), "lost child key {key}");
        }
        for key in [
            "HOME",
            "USERPROFILE",
            "HOMEDRIVE",
            "HOMEPATH",
            "SYSTEMROOT",
            "WINDIR",
            "COMSPEC",
            "PATHEXT",
            "APPDATA",
            "LOCALAPPDATA",
        ] {
            if let Some((_, value)) = std::env::vars().find(|(k, _)| k.eq_ignore_ascii_case(key)) {
                if through_shell && key == "PATHEXT" {
                    let actual = environment.get(key).expect("child PATHEXT");
                    assert!(actual == &value || actual.starts_with(&format!("{value};")));
                    continue;
                }
                assert_eq!(environment.get(key), Some(&value), "lost base key {key}");
            }
        }
    }

    #[test]
    fn relaxed_unenforced_pipe_environment_is_stripped() {
        with_daemon_environment(
            "sandbox_spawn::environment_tests::relaxed_unenforced_pipe_environment_is_stripped",
            || {
                #[cfg(windows)]
                let mut command = {
                    let mut command = Command::new("cmd.exe");
                    command.args(["/d", "/c", "set"]);
                    command
                };
                #[cfg(not(windows))]
                let mut command = Command::new("/usr/bin/env");
                let request = request_environment();
                crate::agent_child_env::apply_to_command(&mut command, &request);
                apply_sandbox_environment(&SpawnPlan::RelaxedUnenforced, &mut command, &request);
                let output = command.output().unwrap();
                assert!(output.status.success());
                let environment = String::from_utf8(output.stdout)
                    .unwrap()
                    .lines()
                    .filter_map(|line| line.split_once('='))
                    .map(|(key, value)| (key.to_ascii_uppercase(), value.to_string()))
                    .collect();
                assert_stripped_environment(environment, false);
            },
        );
    }

    #[test]
    fn relaxed_unenforced_pty_environment_is_stripped() {
        with_daemon_environment(
            "sandbox_spawn::environment_tests::relaxed_unenforced_pty_environment_is_stripped",
            || {
                let dir = tempfile::tempdir().unwrap();
                let (command, _) = pty_command_for_plan(
                    &SpawnPlan::RelaxedUnenforced,
                    OsStr::new("unused-shell"),
                    &[],
                    &dir.path().join("task.json"),
                    dir.path(),
                    &request_environment(),
                )
                .unwrap();
                let environment = command
                    .iter_full_env_as_str()
                    .map(|(key, value)| (key.to_ascii_uppercase(), value.to_string()))
                    .collect();
                assert_stripped_environment(environment, false);
            },
        );
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn isolated_descriptor_test(name: &str) -> bool {
        const KEY: &str = "AFT_TEST_DESCRIPTOR_ISOLATION";
        if std::env::var(KEY).as_deref() == Ok(name) {
            return true;
        }
        // A deliberately inheritable FD must not overlap other tests' forks.
        // Keep its creation and the parent-side snapshot in a one-test process.
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env(KEY, name)
            .output()
            .unwrap();
        println!("{}", String::from_utf8_lossy(&output.stdout));
        eprintln!("{}", String::from_utf8_lossy(&output.stderr));
        assert!(
            output.status.success(),
            "isolated descriptor test failed: {output:?}"
        );
        false
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn high_descriptor_target(hard_limit: u64) -> Option<RawFd> {
        let target = hard_limit.saturating_sub(16).min((RawFd::MAX - 1) as u64);
        (target > 1024).then_some(target as RawFd)
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn high_descriptor_target_respects_the_actual_hard_limit() {
        for hard in [2048, 65_536, 1_048_576] {
            let fd = high_descriptor_target(hard).unwrap();
            assert!(fd > 1024 && (fd as u64) < hard);
        }
        assert_eq!(high_descriptor_target(1024), None);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn high_descriptor_for_test(cloexec: bool) -> Option<OwnedFd> {
        let mut limit = unsafe { std::mem::zeroed::<libc::rlimit>() };
        assert_eq!(
            unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) },
            0
        );
        let maximum = if limit.rlim_max == libc::RLIM_INFINITY {
            #[cfg(target_os = "linux")]
            let maximum = std::fs::read_to_string("/proc/sys/fs/nr_open")
                .unwrap()
                .trim()
                .parse::<u64>()
                .unwrap();
            #[cfg(target_os = "macos")]
            let maximum = {
                let mut maximum: libc::c_int = 0;
                let mut size = std::mem::size_of_val(&maximum);
                assert_eq!(
                    unsafe {
                        libc::sysctlbyname(
                            c"kern.maxfilesperproc".as_ptr(),
                            (&mut maximum as *mut libc::c_int).cast(),
                            &mut size,
                            std::ptr::null_mut(),
                            0,
                        )
                    },
                    0
                );
                u64::try_from(maximum).unwrap()
            };
            maximum
        } else {
            limit.rlim_max as u64
        };
        let Some(target) = high_descriptor_target(maximum) else {
            eprintln!("SKIP high-descriptor test: effective hard RLIMIT_NOFILE={maximum} is too low for a descriptor above 1024 with a safety margin");
            return None;
        };
        limit.rlim_cur = limit.rlim_cur.max((target as u64 + 1) as libc::rlim_t);
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) }, 0);
        let source = tempfile::tempfile().unwrap();
        let operation = if cloexec {
            libc::F_DUPFD_CLOEXEC
        } else {
            libc::F_DUPFD
        };
        let fd = unsafe { libc::fcntl(source.as_raw_fd(), operation, target) };
        assert!(
            fd >= target,
            "cannot allocate high descriptor: {}",
            std::io::Error::last_os_error()
        );
        let high = unsafe { OwnedFd::from_raw_fd(fd) };
        assert!(
            std::fs::metadata(descriptor_path_for_test(fd)).is_ok(),
            "descriptor path must exist before exec"
        );
        assert_eq!(
            unsafe { libc::fcntl(fd, libc::F_GETFD) } & libc::FD_CLOEXEC != 0,
            cloexec
        );
        eprintln!("high-descriptor test: hard={maximum}, fd={fd}, cloexec={cloexec}");
        Some(high)
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn descriptor_path_for_test(fd: RawFd) -> String {
        if cfg!(target_os = "linux") {
            format!("/proc/self/fd/{fd}")
        } else {
            format!("/dev/fd/{fd}")
        }
    }

    #[test]
    fn fallback_fcntl_work_tracks_open_descriptors_not_their_numbers() {
        let open = [7, 70_000];
        let mut gets = 0;
        let mut sets = Vec::new();
        mark_extra_fds_cloexec_fallback_with(&open, |fd, operation, flags| {
            if operation == libc::F_GETFD {
                gets += 1;
                if fd == open[0] {
                    0
                } else if fd == open[1] {
                    libc::FD_CLOEXEC
                } else {
                    unsafe { libc::fcntl(-1, libc::F_GETFD) }
                }
            } else {
                assert_eq!(operation, libc::F_SETFD);
                sets.push((fd, flags));
                0
            }
        })
        .unwrap();
        assert_eq!(
            gets,
            open.len(),
            "fallback must not probe unused descriptor numbers"
        );
        assert_eq!(sets, [(open[0], libc::FD_CLOEXEC)]);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn marker_allowlist_closes_high_fds_and_preserves_markers() {
        if !isolated_descriptor_test(
            "sandbox_spawn::tests::marker_allowlist_closes_high_fds_and_preserves_markers",
        ) {
            return;
        }
        let Some(high) = high_descriptor_for_test(false) else {
            return;
        };
        let exit = tempfile::tempfile().unwrap();
        let failure = tempfile::tempfile().unwrap();
        let mut command = Command::new("/bin/sh");
        command.args([
            "-c",
            "test ! -e \"$1\" || exit 99; printf exit >&3; printf failure >&4",
            "marker-test",
        ]);
        command.arg(descriptor_path_for_test(high.as_raw_fd()));
        apply_marker_fd_allowlist(&mut command, exit.as_raw_fd(), failure.as_raw_fd()).unwrap();
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "high descriptor leaked: {output:?}"
        );
        let mut exit_text = String::new();
        let mut failure_text = String::new();
        let mut exit = exit;
        let mut failure = failure;
        exit.seek(SeekFrom::Start(0)).unwrap();
        failure.seek(SeekFrom::Start(0)).unwrap();
        exit.read_to_string(&mut exit_text).unwrap();
        failure.read_to_string(&mut failure_text).unwrap();
        assert_eq!(exit_text, "exit");
        assert_eq!(failure_text, "failure");
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn fallback_sweep_covers_high_descriptor_range() {
        use std::os::unix::process::CommandExt;
        if !isolated_descriptor_test(
            "sandbox_spawn::tests::fallback_sweep_covers_high_descriptor_range",
        ) {
            return;
        }
        let Some(high) = high_descriptor_for_test(false) else {
            return;
        };
        let open_fds = open_descriptor_snapshot().unwrap();
        assert!(
            open_fds.contains(&high.as_raw_fd()),
            "parent inventory omitted the high descriptor"
        );
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "test ! -e \"$1\"", "fallback-test"]);
        command.arg(descriptor_path_for_test(high.as_raw_fd()));
        unsafe {
            command.pre_exec(move || mark_extra_fds_cloexec_fallback(&open_fds));
        }
        assert!(command.status().unwrap().success());
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn fallback_preserves_cloexec_for_descriptors_opened_after_snapshot() {
        use std::os::unix::process::CommandExt;
        if !isolated_descriptor_test("sandbox_spawn::tests::fallback_preserves_cloexec_for_descriptors_opened_after_snapshot") {
            return;
        }
        let open_fds = open_descriptor_snapshot().unwrap();
        let Some(high) = high_descriptor_for_test(true) else {
            return;
        };
        assert!(!open_fds.contains(&high.as_raw_fd()));
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "test ! -e \"$1\"", "late-open-test"]);
        command.arg(descriptor_path_for_test(high.as_raw_fd()));
        unsafe {
            command.pre_exec(move || mark_extra_fds_cloexec_fallback(&open_fds));
        }
        assert!(command.status().unwrap().success());
    }

    #[test]
    fn shell_containment_requires_strict_native_plan() {
        let project = tempfile::tempdir().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let mut profile = SandboxProfile::build(
            vec![project.path().to_path_buf()],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            temp.path().to_path_buf(),
        )
        .unwrap();
        for mode in [SandboxMode::Off, SandboxMode::Relaxed, SandboxMode::Strict] {
            profile.mode = mode;
            let plan = SpawnPlan::launcher_for_test(profile.clone(), PathBuf::from("aft"));
            assert_eq!(
                plan.enforces_shell_containment(),
                mode == SandboxMode::Strict
            );
        }
        for plan in [
            SpawnPlan::Unsandboxed,
            SpawnPlan::RelaxedUnenforced,
            SpawnPlan::Host {
                shell_path: PathBuf::from("/bin/sh"),
                environment: ChildEnvironment::new(),
            },
            SpawnPlan::refused_for_test("sandbox_unavailable"),
        ] {
            assert!(!plan.enforces_shell_containment(), "{plan:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn relaxed_credentials_follow_tool_environment_paths() {
        let home = Path::new("/operator");
        let roots = vec![PathBuf::from("/checkout")];
        let environment = HashMap::from([
            ("CARGO_HOME", "/custom/cargo"),
            ("GNUPGHOME", "/custom/gnupg"),
            ("AWS_CONFIG_FILE", "/custom/aws/config"),
            ("AWS_SHARED_CREDENTIALS_FILE", "/custom/aws/credentials"),
            ("DOCKER_CONFIG", "/custom/docker"),
            ("KUBECONFIG", "/custom/kube/one:/custom/kube/two"),
            ("GH_CONFIG_DIR", "/custom/gh"),
            ("XDG_CONFIG_HOME", "/custom/xdg"),
            ("CLOUDSDK_CONFIG", "/custom/gcloud"),
        ]);
        let (denies, cargo_homes) =
            relaxed_credential_paths(home, &roots, |key| environment.get(key).map(OsString::from))
                .unwrap();
        assert!(cargo_homes.contains(&PathBuf::from("/custom/cargo")));
        assert!(cargo_homes.contains(&home.join(".cargo")));
        for expected in [
            "/custom/gnupg",
            "/custom/aws/config",
            "/custom/aws/credentials",
            "/custom/docker/config.json",
            "/custom/kube/one",
            "/custom/kube/two",
            "/custom/gh",
            "/custom/xdg/gh",
            "/custom/xdg/gcloud",
            "/custom/gcloud",
        ] {
            assert!(
                denies.contains(&PathBuf::from(expected)),
                "missing {expected}: {denies:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn relaxed_credentials_resolve_relative_settings_for_each_checkout() {
        let roots = [PathBuf::from("/one"), PathBuf::from("/two")];
        let (denies, cargo_homes) =
            relaxed_credential_paths(Path::new("/operator"), &roots, |key| match key {
                "CARGO_HOME" => Some("cargo".into()),
                "KUBECONFIG" => Some(":kube/a::kube/b:".into()),
                "GNUPGHOME" => Some(OsString::new()),
                _ => None,
            })
            .unwrap();
        for root in roots {
            assert!(cargo_homes.contains(&root.join("cargo")));
            assert!(denies.contains(&root.join("kube/a")));
            assert!(denies.contains(&root.join("kube/b")));
            assert!(
                !denies.contains(&root),
                "empty entries must not deny the checkout"
            );
        }
    }

    fn credential_profile(
        temp: &Path,
        denies: Vec<PathBuf>,
        cargo_homes: Vec<PathBuf>,
    ) -> SandboxProfile {
        let mut profile = SandboxProfile::build(
            vec![temp.to_path_buf()],
            vec![],
            vec![],
            vec![],
            vec![],
            vec![],
            vec![],
            temp.to_path_buf(),
        )
        .unwrap();
        profile.mode = SandboxMode::Relaxed;
        profile.read_deny = denies.clone();
        profile.write_deny = denies;
        add_relaxed_cargo_credential_denies(&mut profile, cargo_homes).unwrap();
        profile.canonicalize_for_launch().unwrap()
    }

    #[test]
    fn relaxed_credentials_existing_dotdot_resolves_before_profile() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        std::fs::create_dir_all(base.join("target/nested")).unwrap();
        std::fs::write(base.join("target/config"), "secret").unwrap();
        std::os::unix::fs::symlink(base.join("target/nested"), base.join("alias")).unwrap();
        let input = base.join("alias/../config");
        let (denies, cargo) = relaxed_credential_paths(&base, &[base.clone()], |key| {
            (key == "AWS_CONFIG_FILE").then(|| input.clone().into_os_string())
        })
        .unwrap();
        assert!(denies.contains(&base.join("target/config")), "{denies:?}");
        assert!(!denies.contains(&input), "unnormalized alias: {denies:?}");
        let profile = credential_profile(&base, denies, cargo);
        assert!(profile.read_deny.contains(&base.join("target/config")));
    }

    #[test]
    fn relaxed_credentials_missing_dotdot_resolves_existing_ancestor() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        std::fs::create_dir_all(base.join("target/nested")).unwrap();
        std::os::unix::fs::symlink(base.join("target/nested"), base.join("alias")).unwrap();
        let input = base.join("alias/../missing/config");
        let (denies, cargo) = relaxed_credential_paths(&base, &[base.clone()], |key| {
            (key == "AWS_SHARED_CREDENTIALS_FILE").then(|| input.clone().into_os_string())
        })
        .unwrap();
        assert!(
            denies.contains(&base.join("target/missing/config")),
            "{denies:?}"
        );
        let profile = credential_profile(&base, denies, cargo);
        assert!(profile
            .read_deny
            .contains(&base.join("target/missing/config")));
    }

    #[test]
    fn relaxed_credentials_symlink_target_and_literal_are_denied() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        std::fs::create_dir(base.join("target")).unwrap();
        let alias = base.join("alias");
        std::os::unix::fs::symlink(base.join("target"), &alias).unwrap();
        let (denies, cargo) = relaxed_credential_paths(&base, &[base.clone()], |key| {
            (key == "GH_CONFIG_DIR").then(|| alias.clone().into_os_string())
        })
        .unwrap();
        assert!(denies.contains(&base.join("target")), "{denies:?}");
        assert!(denies.contains(&alias), "{denies:?}");
        let profile = credential_profile(&base, denies, cargo);
        assert!(profile.read_deny.contains(&base.join("target")));
        assert!(profile.write_deny.contains(&alias));
    }

    #[test]
    fn relaxed_credentials_all_environment_denies_resolve_dotdot() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        std::fs::create_dir(base.join("nested")).unwrap();
        for (key, suffixes) in [
            ("CARGO_HOME", vec!["credentials"]),
            ("GNUPGHOME", vec![""]),
            ("GH_CONFIG_DIR", vec![""]),
            ("CLOUDSDK_CONFIG", vec![""]),
            ("AWS_CONFIG_FILE", vec![""]),
            ("AWS_SHARED_CREDENTIALS_FILE", vec![""]),
            ("DOCKER_CONFIG", vec!["config.json"]),
            ("XDG_CONFIG_HOME", vec!["gh", "gcloud"]),
            ("KUBECONFIG", vec![""]),
        ] {
            let input = PathBuf::from("nested/../missing");
            let (denies, cargo) = relaxed_credential_paths(&base, &[base.clone()], |name| {
                (name == key).then(|| input.clone().into_os_string())
            })
            .unwrap();
            let profile = credential_profile(&base, denies, cargo);
            for suffix in suffixes {
                let expected = if suffix.is_empty() {
                    base.join("missing")
                } else {
                    base.join("missing").join(suffix)
                };
                let paths = if key == "CARGO_HOME" {
                    &profile.deny_prefixes
                } else {
                    &profile.read_deny
                };
                assert!(paths.contains(&expected), "{key}: {paths:?}");
            }
        }
        for key in ["XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME"] {
            let paths = resolved_env_deny_paths(key, base.join("nested/../missing/cortexkit"));
            assert_eq!(paths, vec![base.join("missing/cortexkit")], "{key}");
        }
    }

    #[test]
    fn relaxed_credentials_unresolvable_entry_refuses_the_floor() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let link = base.join("loop");
        std::os::unix::fs::symlink(&link, &link).unwrap();
        let input = std::env::join_paths([link.clone(), base.join("safe")]).unwrap();
        let error = crate::sandbox_profile::resolve_env_deny_paths(link.clone(), "KUBECONFIG")
            .unwrap_err()
            .to_string();
        assert!(error.contains("KUBECONFIG"), "{error}");
        let error = relaxed_credential_paths(&base, &[base.clone()], |key| {
            (key == "KUBECONFIG").then(|| input.clone())
        })
        .unwrap_err();
        assert!(error.contains("KUBECONFIG"), "{error}");
    }

    #[test]
    fn relaxed_credentials_never_collapse_dotdot_across_unresolved_symlink() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        std::os::unix::fs::symlink(base.join("missing/child"), base.join("alias")).unwrap();
        let paths = resolved_env_deny_paths("AWS_CONFIG_FILE", base.join("alias/../config"));
        assert!(paths.is_empty(), "unsafe lexical resolution: {paths:?}");
    }

    #[test]
    fn relaxed_credentials_cargo_entries_refuse_unresolvable_and_deny_valid_targets() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let cargo_home = base.join("cargo");
        std::fs::create_dir(&cargo_home).unwrap();
        let secret = base.join("secret");
        std::fs::write(&secret, "cargo-secret").unwrap();
        std::os::unix::fs::symlink(&secret, cargo_home.join("credentials.toml")).unwrap();
        let looping = cargo_home.join("credentials");
        std::os::unix::fs::symlink(&looping, &looping).unwrap();
        let (denies, cargo) = relaxed_credential_paths(&base, &[base.clone()], |key| {
            (key == "CARGO_HOME").then(|| cargo_home.clone().into_os_string())
        })
        .unwrap();
        let mut invalid = SandboxProfile::build(
            vec![base.clone()],
            vec![],
            vec![],
            vec![],
            vec![],
            vec![],
            vec![],
            base.clone(),
        )
        .unwrap();
        assert!(add_relaxed_cargo_credential_denies(&mut invalid, cargo.clone()).is_err());
        std::fs::remove_file(&looping).unwrap();
        let profile = credential_profile(&base, denies, cargo);
        assert!(profile.read_deny.contains(&secret), "{profile:?}");
        assert!(profile
            .write_deny
            .contains(&cargo_home.join("credentials.toml")));
        assert!(profile.deny_prefixes.contains(&looping));
    }

    struct SupervisorRun {
        code: Option<i32>,
        exit_marker: String,
        failure_marker: String,
    }

    /// Run the marker-fd supervisor with a fake launcher script, the given
    /// request environment and shell, and return what it recorded.
    fn run_supervisor(
        launcher_body: &str,
        target: &[&str],
        request: &HashMap<String, String>,
        shell: &[&str],
    ) -> SupervisorRun {
        use std::os::fd::AsRawFd;
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let temp = project.join("temp");
        std::fs::create_dir_all(&temp).unwrap();
        let launcher = root.path().join("launcher");
        crate::executable_cache::write_executable_with_mode(
            &launcher,
            launcher_body.as_bytes(),
            0o700,
        )
        .unwrap();
        let profile = SandboxProfile::build(
            vec![project],
            vec![],
            vec![],
            vec![],
            vec![],
            vec![],
            vec![],
            temp,
        )
        .unwrap();
        let plan = SpawnPlan::launcher_for_test(profile.clone(), launcher.clone());
        let exit_path = root.path().join("exit");
        let failure_path = root.path().join("failure");
        let exit_file = File::create(&exit_path).unwrap();
        let failure_file = File::create(&failure_path).unwrap();
        let target = target.iter().map(OsString::from).collect::<Vec<_>>();
        let mut command = Command::new(shell[0]);
        command.args(&shell[1..]);
        let fds = apply_marker_fd_allowlist(
            &mut command,
            exit_file.as_raw_fd(),
            failure_file.as_raw_fd(),
        )
        .unwrap();
        let (_, args, _) = launcher_argv(
            &profile,
            &launcher,
            target[0].as_os_str(),
            &target[1..],
            Path::new("unused"),
            Some(fds),
            None,
        )
        .unwrap();
        command.args(args);
        apply_sandbox_environment(&plan, &mut command, request);
        let status = command.status().unwrap();
        SupervisorRun {
            code: status.code(),
            exit_marker: std::fs::read_to_string(exit_path).unwrap(),
            failure_marker: std::fs::read_to_string(failure_path).unwrap(),
        }
    }

    /// The fake launcher's argv is `sandbox-launch --profile-json <json>
    /// --failure-fd <fd> -- <target...>`.
    const FAILING_LAUNCHER: &str = "#!/bin/sh\nprintf %s sandbox_unavailable >&\"$5\"\nexit 78\n";
    const EXEC_LAUNCHER: &str = "#!/bin/sh\nshift 6\nexec \"$@\"\n";

    #[test]
    fn supervisor_reports_unavailable_only_when_the_launcher_says_so() {
        let failed = run_supervisor(
            FAILING_LAUNCHER,
            &["/bin/true"],
            &HashMap::new(),
            &["/bin/sh"],
        );
        assert_eq!(failed.code, Some(78));
        assert_eq!(failed.failure_marker, "sandbox_unavailable");
        assert_eq!(failed.exit_marker, "78");

        // A target that exits 78 after a successful launch is an ordinary
        // command status, not a confinement failure.
        let target = run_supervisor(
            EXEC_LAUNCHER,
            &["/bin/sh", "-c", "exit 78"],
            &HashMap::new(),
            &["/bin/sh"],
        );
        assert_eq!(target.code, Some(78));
        assert_eq!(target.failure_marker, "");
        assert_eq!(target.exit_marker, "");
    }

    #[test]
    fn exported_function_cannot_run_in_outer_supervisor() {
        let sentinel = tempfile::tempdir().unwrap();
        let sentinel = sentinel.path().join("unconfined-function");
        let request = HashMap::from([
            (
                "BASH_FUNC_printf%%".to_string(),
                "() { echo escaped > \"$SENTINEL\"; }".to_string(),
            ),
            (
                "SENTINEL".to_string(),
                sentinel.to_string_lossy().into_owned(),
            ),
        ]);
        // Bash in POSIX mode stands in for a Bash-backed /bin/sh (as on
        // macOS) even on Linux hosts where /bin/sh is dash, which does not
        // import exported functions. The supervisor's own `printf` runs
        // outside confinement after the launcher fails.
        let run = run_supervisor(
            FAILING_LAUNCHER,
            &["/bin/true"],
            &request,
            &["bash", "--posix"],
        );
        assert_eq!(run.code, Some(78));
        assert!(
            !sentinel.exists(),
            "request function ran outside the launcher"
        );
        assert_eq!(run.failure_marker, "sandbox_unavailable");
        assert_eq!(run.exit_marker, "78");
    }

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

#[cfg(all(test, unix))]
mod git_policy_cache_tests {
    use super::*;

    struct Fixture {
        _dir: tempfile::TempDir,
        root: PathBuf,
        roots: Mutex<HashSet<PathBuf>>,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().canonicalize().unwrap();
            let environment = vec![
                (OsString::from("HOME"), root.clone().into_os_string()),
                (
                    OsString::from("XDG_CONFIG_HOME"),
                    root.join("xdg").into_os_string(),
                ),
                (
                    OsString::from("GIT_CONFIG_GLOBAL"),
                    root.join("global").into_os_string(),
                ),
                (
                    OsString::from("GIT_CONFIG_SYSTEM"),
                    root.join("system").into_os_string(),
                ),
            ];
            git_policy_test_states()
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .insert(
                    root.clone(),
                    GitPolicyTestState {
                        environment,
                        ..Default::default()
                    },
                );
            let output = git_policy_output(&root, &["init", "--quiet"]).unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let fixture = Self {
                _dir: dir,
                roots: Mutex::new(HashSet::from([root.clone()])),
                root,
            };
            fixture.hooks("first-hooks");
            fixture.state(|state| state.calls = 0);
            fixture
        }

        fn hooks(&self, path: &str) {
            std::fs::create_dir_all(self.root.join(path)).unwrap();
            std::fs::write(
                self.root.join(".git/config"),
                format!(
                    "[core]\n\trepositoryformatversion = 0\n\tbare = false\n\thooksPath = {path}\n"
                ),
            )
            .unwrap();
        }

        fn resolve(&self) -> (GitPolicy, usize) {
            self.resolve_at(&self.root)
        }

        fn resolve_at(&self, root: &Path) -> (GitPolicy, usize) {
            self.register_root(root);
            let before = with_git_policy_test_state(root, |state| state.calls).unwrap();
            let policy = with_worker_jail(true, || resolve_git_policy(root)).unwrap();
            (
                policy,
                with_git_policy_test_state(root, |state| state.calls).unwrap() - before,
            )
        }

        fn state<R>(&self, run: impl FnOnce(&mut GitPolicyTestState) -> R) -> R {
            with_git_policy_test_state(&self.root, run).unwrap()
        }

        fn register_root(&self, root: &Path) {
            let root = root.canonicalize().unwrap();
            let environment = self.state(|state| state.environment.clone());
            git_policy_test_states()
                .lock()
                .unwrap()
                .entry(root.clone())
                .or_insert_with(|| GitPolicyTestState {
                    environment,
                    ..Default::default()
                });
            self.roots.lock().unwrap().insert(root);
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            for root in self.roots.lock().unwrap().iter() {
                let environment = git_policy_environment(root);
                for worker in [false, true] {
                    if let Some(key) = git_policy_cache_key(root, &environment, worker) {
                        git_policy_cache().lock().unwrap().remove(&key);
                    }
                }
                git_policy_test_states().lock().unwrap().remove(root);
            }
        }
    }

    #[test]
    fn git_policy_cache_is_bounded_and_evicts_least_recent_entries() {
        fn key(index: usize) -> GitPolicyCacheKey {
            GitPolicyCacheKey {
                worktree: PathBuf::from(format!("worktree-{index}")),
                git_dir: PathBuf::from(format!("git-{index}")),
                common_dir: PathBuf::from(format!("common-{index}")),
                environment: Vec::new(),
                worker: false,
            }
        }

        fn value() -> CachedGitPolicy {
            CachedGitPolicy {
                policy: GitPolicy {
                    read_roots: Vec::new(),
                    hooks: Vec::new(),
                    linked: None,
                    configured_paths: Vec::new(),
                    metadata_allow: Vec::new(),
                },
                inputs: GitPolicyInputs {
                    environment: Vec::new(),
                    executable: PathBuf::new(),
                    files: BTreeMap::new(),
                    resolved_paths: Vec::new(),
                },
            }
        }

        let mut cache = GitPolicyCache::new();
        for index in 0..=256 {
            cache.insert(key(index), value());
        }
        assert_eq!(cache.entries.len(), 256);
        assert!(cache.entries.contains_key(&key(256)));
        assert!(!cache.entries.contains_key(&key(0)));

        assert!(cache.get(&key(1)).is_some());
        cache.insert(key(257), value());
        assert!(cache.entries.contains_key(&key(1)));
        assert!(!cache.entries.contains_key(&key(2)));
    }

    #[test]
    fn git_policy_cache_second_spawn_uses_zero_git_queries() {
        let fixture = Fixture::new();
        let (first, cold) = fixture.resolve();
        assert!(cold > 0);
        let (second, warm) = fixture.resolve();
        assert_eq!(second.hooks, first.hooks);
        assert_eq!(warm, 0, "unchanged policy must not spawn git");
    }

    #[test]
    fn git_policy_cache_review_branch_switch_refreshes_hooks() {
        let fixture = Fixture::new();
        let included = fixture.root.join("branch-config");
        std::fs::write(&included, "[core]\n hooksPath = branch-hooks\n").unwrap();
        let config = fixture.root.join(".git/config");
        let mut contents = std::fs::read_to_string(&config).unwrap();
        contents.push_str(&format!(
            "[includeIf \"onbranch:review-selected\"]\n path = {}\n",
            included.display()
        ));
        std::fs::write(config, contents).unwrap();
        // An unborn branch is enough: checkout updates HEAD without a commit.
        for worker in [false, true] {
            for (branch, hooks) in [
                ("review-other", "first-hooks"),
                ("review-selected", "branch-hooks"),
                ("review-other", "first-hooks"),
            ] {
                let output =
                    git_policy_output(&fixture.root, &["checkout", "--quiet", "--orphan", branch])
                        .unwrap();
                assert!(output.status.success(), "{:?}", output);
                let policy =
                    with_worker_jail(worker, || resolve_git_policy(&fixture.root)).unwrap();
                assert_eq!(policy.hooks, vec![fixture.root.join(hooks)]);
                let before = fixture.state(|state| state.calls);
                with_worker_jail(worker, || resolve_git_policy(&fixture.root)).unwrap();
                assert_eq!(fixture.state(|state| state.calls), before);
            }
        }
    }

    #[test]
    fn git_policy_cache_review_include_discovery_race_is_not_published() {
        let fixture = Fixture::new();
        let included = fixture.root.join("included");
        let old = fixture.root.join("old-include");
        let new = fixture.root.join("new-include");
        std::fs::write(&old, "[core]\n hooksPath = old-hooks\n").unwrap();
        std::fs::write(&new, "[core]\n hooksPath = new-hooks\n").unwrap();
        std::fs::write(&included, format!("[include]\n path = {}\n", old.display())).unwrap();
        std::fs::write(
            fixture.root.join(".git/config"),
            format!("[include]\n path = {}\n", included.display()),
        )
        .unwrap();
        fixture.state(|state| {
            state.after_config_listing = Some(Box::new(move || {
                std::fs::write(included, format!("[include]\n path = {}\n", new.display()))
                    .unwrap();
            }))
        });
        assert_eq!(
            fixture.resolve().0.hooks,
            vec![fixture.root.join("new-hooks")]
        );
        assert!(
            fixture.resolve().1 > 0,
            "changed discovery must not publish a cache entry"
        );
        assert_eq!(
            fixture.resolve().1,
            0,
            "stable discovery should cache again"
        );
        std::fs::write(
            fixture.root.join("new-include"),
            "[core]\n hooksPath = changed-new-hooks\n",
        )
        .unwrap();
        assert_eq!(
            fixture.resolve().0.hooks,
            vec![fixture.root.join("changed-new-hooks")]
        );
    }

    #[test]
    fn git_policy_cache_review_unrelated_roots_cannot_consume_injections() {
        let fixture = Fixture::new();
        let other = tempfile::tempdir().unwrap();
        let other_root = other.path().canonicalize().unwrap();
        assert!(git_policy_output(&other_root, &["init", "--quiet"])
            .unwrap()
            .status
            .success());
        fixture.state(|state| {
            state.environment.extend([
                ("GIT_CONFIG_COUNT".into(), "1".into()),
                (
                    "GIT_CONFIG_KEY_0".into(),
                    "aftcachefixture.isolatedvalue".into(),
                ),
                ("GIT_CONFIG_VALUE_0".into(), "fixture-only-value".into()),
            ])
        });
        let before = fixture.state(|state| state.calls);
        let output = git_policy_output(
            &other_root,
            &["config", "--get", "aftcachefixture.isolatedvalue"],
        )
        .unwrap();
        let calls_unchanged = fixture.state(|state| state.calls) == before;
        let environment_isolated = !output.status.success();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (waiter_tx, waiter_rx) = std::sync::mpsc::channel();
        release_tx.send(true).unwrap();
        fixture.state(|state| {
            state.uncached_gate = Some(GitPolicyTestUncachedGate {
                entered: entered_tx,
                release: release_rx,
            });
            state.waiter_signal = Some(waiter_tx);
            state.delay_ms = 30;
        });
        signal_git_policy_waiter_for_test(&other_root);
        let result = std::panic::catch_unwind(|| resolve_git_policy(&other_root));
        let gate_isolated = entered_rx.try_recv().is_err();
        let waiter_isolated = waiter_rx.try_recv().is_err();
        let delay_isolated =
            with_git_policy_test_state(&other_root, |state| state.delay_ms).is_none();
        assert_eq!(
            (
                calls_unchanged,
                environment_isolated,
                result.is_ok(),
                gate_isolated,
                waiter_isolated,
                delay_isolated
            ),
            (true, true, true, true, true, true),
            "unrelated launches must not use another root's test state"
        );
        let alias = fixture.root.join("root-alias");
        std::os::unix::fs::symlink(&fixture.root, &alias).unwrap();
        assert_eq!(
            git_policy_environment(&alias),
            git_policy_environment(&fixture.root)
        );
        assert_eq!(
            with_git_policy_test_state(&alias, |state| state.delay_ms),
            Some(30)
        );
    }

    #[test]
    fn git_policy_cache_include_identity_change_during_second_discovery_is_not_published() {
        let fixture = Fixture::new();
        let included = fixture.root.join("included");
        std::fs::write(&included, "[core]\n hooksPath = old-hooks\n").unwrap();
        std::fs::write(
            fixture.root.join(".git/config"),
            format!("[include]\n path = {}\n", included.display()),
        )
        .unwrap();
        let root = fixture.root.clone();
        fixture.state(|state| {
            state.after_config_listing = Some(Box::new(move || {
                with_git_policy_test_state(&root, |state| {
                    state.after_config_listing = Some(Box::new(move || {
                        std::fs::write(included, "[core]\n hooksPath = changed-hooks\n").unwrap();
                    }));
                })
                .unwrap();
            }))
        });
        assert_eq!(
            fixture.resolve().0.hooks,
            vec![fixture.root.join("changed-hooks")]
        );
        assert!(
            fixture.resolve().1 > 0,
            "changed include identity must not publish a cache entry"
        );
        assert_eq!(fixture.resolve().1, 0);
    }

    #[test]
    fn git_policy_cache_removed_include_during_discovery_is_not_published() {
        let fixture = Fixture::new();
        let included = fixture.root.join("included");
        let removed = fixture.root.join("removed");
        std::fs::write(&removed, "[core]\n hooksPath = old-hooks\n").unwrap();
        std::fs::write(
            &included,
            format!("[include]\n path = {}\n", removed.display()),
        )
        .unwrap();
        std::fs::write(
            fixture.root.join(".git/config"),
            format!("[include]\n path = {}\n", included.display()),
        )
        .unwrap();
        fixture.state(|state| {
            state.after_config_listing = Some(Box::new(move || {
                std::fs::write(included, "").unwrap();
            }))
        });
        assert_eq!(
            fixture.resolve().0.hooks,
            vec![fixture.root.join(".git/hooks")]
        );
        assert!(
            fixture.resolve().1 > 0,
            "removed discovery dependency must not publish a cache entry"
        );
        assert_eq!(fixture.resolve().1, 0);
    }

    #[test]
    fn git_policy_cache_review_valueless_boolean_is_cached() {
        let fixture = Fixture::new();
        let config = fixture.root.join(".git/config");
        let mut contents = std::fs::read_to_string(&config).unwrap();
        contents.push_str("[core]\n logAllRefUpdates\n");
        std::fs::write(config, contents).unwrap();
        let (policy, cold) = fixture.resolve();
        assert!(cold > 0);
        assert_eq!(policy.hooks, vec![fixture.root.join("first-hooks")]);
        assert_eq!(
            fixture.resolve().1,
            0,
            "valueless booleans must not disable caching"
        );
    }

    #[test]
    fn git_policy_cache_gitdir_conditions_track_metadata_location() {
        for condition in ["gitdir", "gitdir/i"] {
            let fixture = Fixture::new();
            let included = fixture.root.join("location-config");
            std::fs::write(&included, "[core]\n hooksPath = location-hooks\n").unwrap();
            let config = fixture.root.join(".git/config");
            let mut contents = std::fs::read_to_string(&config).unwrap();
            let pattern = fixture.root.join(".git").to_str().unwrap().to_owned();
            let pattern = if condition == "gitdir/i" {
                pattern.to_uppercase()
            } else {
                pattern
            };
            contents.push_str(&format!(
                "[includeIf \"{condition}:{pattern}\"]\n path = {}\n",
                included.display()
            ));
            std::fs::write(config, contents).unwrap();
            assert_eq!(
                fixture.resolve().0.hooks,
                vec![fixture.root.join("location-hooks")]
            );
            assert_eq!(fixture.resolve().1, 0);
            std::fs::rename(fixture.root.join(".git"), fixture.root.join("moved-git")).unwrap();
            std::fs::write(fixture.root.join(".git"), "gitdir: moved-git\n").unwrap();
            let (policy, calls) = fixture.resolve();
            assert!(calls > 0);
            assert_eq!(policy.hooks, vec![fixture.root.join("first-hooks")]);
            assert_eq!(fixture.resolve().1, 0);
        }
    }

    #[test]
    fn git_policy_cache_hasconfig_remote_urls_track_config_files() {
        for in_include in [false, true] {
            let fixture = Fixture::new();
            let included = fixture.root.join("remote-hooks-config");
            std::fs::write(&included, "[core]\n hooksPath = remote-hooks\n").unwrap();
            let config = fixture.root.join(".git/config");
            let mut contents = std::fs::read_to_string(&config).unwrap();
            contents.push_str(&format!(
                "[includeIf \"hasconfig:remote.*.url:https://example.invalid/**\"]\n path = {}\n",
                included.display()
            ));
            let remote_config = if in_include {
                let remote_config = fixture.root.join("remote-urls");
                std::fs::write(&remote_config, "").unwrap();
                contents.push_str(&format!("[include]\n path = {}\n", remote_config.display()));
                std::fs::write(&config, contents).unwrap();
                remote_config
            } else {
                std::fs::write(&config, &contents).unwrap();
                config
            };
            let baseline = std::fs::read_to_string(&remote_config).unwrap();
            for (url, expected) in [
                ("https://elsewhere.invalid/repo", "first-hooks"),
                ("https://example.invalid/repo", "remote-hooks"),
                ("https://elsewhere.invalid/repo", "first-hooks"),
            ] {
                std::fs::write(
                    &remote_config,
                    format!("{baseline}[remote \"origin\"]\n url = {url}\n"),
                )
                .unwrap();
                let (policy, calls) = fixture.resolve();
                assert!(calls > 0);
                assert_eq!(policy.hooks, vec![fixture.root.join(expected)]);
                assert_eq!(fixture.resolve().1, 0);
            }
        }
    }

    #[test]
    fn git_policy_cache_unknown_include_conditions_are_not_cached() {
        let fixture = Fixture::new();
        let config = fixture.root.join(".git/config");
        let mut contents = std::fs::read_to_string(&config).unwrap();
        contents
            .push_str("[includeIf \"future-condition:untracked-state\"]\n path = future-config\n");
        std::fs::write(config, contents).unwrap();
        assert!(fixture.resolve().1 > 0);
        assert!(fixture.resolve().1 > 0);
    }

    #[test]
    fn git_policy_cache_changed_config_reports_new_not_stale_hooks() {
        let fixture = Fixture::new();
        let (old, cold) = fixture.resolve();
        assert_eq!(fixture.resolve().1, 0);
        fixture.hooks("changed-longer-hooks");
        let (new, changed) = fixture.resolve();
        assert_eq!(
            new.hooks,
            vec![fixture.root.join("changed-longer-hooks")],
            "stale hooks path after common config change: {:?}",
            new.hooks
        );
        assert_ne!(new.hooks, old.hooks);
        assert_eq!(changed, cold, "one complete re-resolution is required");
        assert_eq!(fixture.resolve().1, 0);
    }

    #[test]
    fn git_policy_cache_include_file_changes_invalidate_cached_policy() {
        let fixture = Fixture::new();
        let included = fixture.root.join("included");
        std::fs::write(&included, "[core]\n hooksPath = included-hooks\n").unwrap();
        std::fs::create_dir(fixture.root.join("included-hooks")).unwrap();
        std::fs::write(
            fixture.root.join(".git/config"),
            format!("[include]\n path = {}\n", included.display()),
        )
        .unwrap();
        let (first, cold) = fixture.resolve();
        assert_eq!(first.hooks, vec![fixture.root.join("included-hooks")]);
        assert!(cold > 0);
        assert_eq!(
            fixture.resolve().1,
            0,
            "unchanged include config should be cached"
        );
        std::fs::write(
            &included,
            "[core]\n hooksPath = replacement-included-hooks\n",
        )
        .unwrap();
        let (changed, calls) = fixture.resolve();
        assert!(calls > 0);
        assert_eq!(
            changed.hooks,
            vec![fixture.root.join("replacement-included-hooks")]
        );
    }

    #[test]
    fn git_policy_cache_global_system_worktree_and_environment_invalidate() {
        let fixture = Fixture::new();
        fixture.resolve();
        for name in ["global", "system"] {
            std::fs::write(
                fixture.root.join(name),
                "[mason]\n originalHooksPath = policy-hooks\n",
            )
            .unwrap();
            let (policy, calls) = fixture.resolve();
            assert!(calls > 0, "creating {name} config must invalidate");
            assert!(policy.configured_paths.contains(&(
                "mason.originalHooksPath".into(),
                fixture.root.join("policy-hooks")
            )));
            assert_eq!(fixture.resolve().1, 0);
        }
        let config = fixture.root.join(".git/config");
        let mut text = std::fs::read_to_string(&config).unwrap();
        text.push_str("[extensions]\n worktreeConfig = true\n");
        std::fs::write(config, text).unwrap();
        fixture.resolve();
        std::fs::write(
            fixture.root.join(".git/config.worktree"),
            "[core]\n hooksPath = worktree-hooks\n",
        )
        .unwrap();
        let (policy, calls) = fixture.resolve();
        assert!(calls > 0);
        assert_eq!(policy.hooks, vec![fixture.root.join("worktree-hooks")]);
        assert_eq!(fixture.resolve().1, 0);
        fixture.state(|state| {
            state.environment.extend([
                ("GIT_CONFIG_COUNT".into(), "1".into()),
                ("GIT_CONFIG_KEY_0".into(), "core.hooksPath".into()),
                ("GIT_CONFIG_VALUE_0".into(), "environment-hooks".into()),
            ])
        });
        let (policy, calls) = fixture.resolve();
        assert!(calls > 0);
        assert_eq!(policy.hooks, vec![fixture.root.join("environment-hooks")]);
        assert_eq!(fixture.resolve().1, 0);
    }

    #[test]
    fn git_policy_cache_keeps_linked_worktrees_separate() {
        let fixture = Fixture::new();
        fixture.hooks("main-hooks");
        let main_config = fixture.root.join(".git/config");
        let mut config = std::fs::read_to_string(&main_config).unwrap();
        config.push_str("[extensions]\n worktreeConfig = true\n");
        std::fs::write(main_config, config).unwrap();

        let commit = git_policy_output(
            &fixture.root,
            &[
                "-c",
                "user.name=Cache Test",
                "-c",
                "user.email=cache@example.invalid",
                "commit",
                "--quiet",
                "--allow-empty",
                "-m",
                "cache fixture",
            ],
        )
        .unwrap();
        assert!(
            commit.status.success(),
            "{}",
            String::from_utf8_lossy(&commit.stderr)
        );
        let linked = fixture.root.join("linked-worktree");
        let added = git_policy_output(
            &fixture.root,
            &[
                "worktree",
                "add",
                "--detach",
                "--quiet",
                linked.to_str().unwrap(),
                "HEAD",
            ],
        )
        .unwrap();
        assert!(
            added.status.success(),
            "{}",
            String::from_utf8_lossy(&added.stderr)
        );
        let pointer = std::fs::read_to_string(linked.join(".git")).unwrap();
        let git_dir = PathBuf::from(pointer.trim().strip_prefix("gitdir:").unwrap().trim());
        let git_dir = if git_dir.is_absolute() {
            git_dir
        } else {
            linked.join(git_dir)
        };
        std::fs::write(
            git_dir.join("config.worktree"),
            format!(
                "[core]\n hooksPath = {}\n",
                linked.join("linked-hooks").display()
            ),
        )
        .unwrap();

        let (main_policy, main_cold) = fixture.resolve_at(&fixture.root);
        let (linked_policy, linked_cold) = fixture.resolve_at(&linked);
        assert!(main_cold > 0);
        assert!(linked_cold > 0);
        assert_eq!(main_policy.hooks, vec![fixture.root.join("main-hooks")]);
        assert_eq!(linked_policy.hooks, vec![linked.join("linked-hooks")]);
        assert_eq!(fixture.resolve_at(&fixture.root).1, 0);
        assert_eq!(fixture.resolve_at(&linked).1, 0);

        let branch_config = fixture.root.join("linked-branch-config");
        std::fs::write(
            &branch_config,
            format!(
                "[core]\n hooksPath = {}\n",
                linked.join("branch-hooks").display()
            ),
        )
        .unwrap();
        let worktree_config = git_dir.join("config.worktree");
        let mut contents = std::fs::read_to_string(&worktree_config).unwrap();
        contents.push_str(&format!(
            "[includeIf \"onbranch:linked-policy\"]\n path = {}\n",
            branch_config.display()
        ));
        std::fs::write(worktree_config, contents).unwrap();
        assert_eq!(fixture.resolve_at(&linked).0.hooks, linked_policy.hooks);
        assert_eq!(fixture.resolve_at(&linked).1, 0);
        let checkout =
            git_policy_output(&linked, &["checkout", "--quiet", "-b", "linked-policy"]).unwrap();
        assert!(checkout.status.success(), "{:?}", checkout);
        let (policy, calls) = fixture.resolve_at(&linked);
        assert!(calls > 0);
        assert_eq!(policy.hooks, vec![linked.join("branch-hooks")]);
        assert_eq!(fixture.resolve_at(&linked).1, 0);
        assert_eq!(
            fixture.resolve_at(&fixture.root).1,
            0,
            "linked HEAD must not invalidate the main worktree"
        );
    }

    #[test]
    fn git_policy_cache_single_flights_concurrent_cold_misses() {
        let fixture = Fixture::new();
        let expected_queries = fixture.resolve().1;
        assert!(expected_queries > 0);
        let key = git_policy_cache_key(&fixture.root, &git_policy_environment(&fixture.root), true)
            .unwrap();
        git_policy_cache().lock().unwrap().remove(&key);
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (waiter_signal_tx, waiter_signal_rx) = std::sync::mpsc::channel();
        fixture.state(|state| {
            state.calls = 0;
            state.uncached_gate = Some(GitPolicyTestUncachedGate {
                entered: entered_tx,
                release: release_rx,
            });
            state.waiter_signal = Some(waiter_signal_tx);
        });
        let owner_root = fixture.root.clone();
        let owner =
            std::thread::spawn(move || with_worker_jail(true, || resolve_git_policy(&owner_root)));
        entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("miss owner did not enter uncached resolution");
        let waiter_root = fixture.root.clone();
        let waiter =
            std::thread::spawn(move || with_worker_jail(true, || resolve_git_policy(&waiter_root)));
        waiter_signal_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("same-key caller did not wait for the miss owner");

        // Model a waiter scheduled after its deadline, even though the owner
        // published a valid entry before the waiter reacquired the cache lock.
        fixture.state(|state| state.expire_wait_on_owner_release = true);
        release_tx.send(false).unwrap();
        let owner_policy = owner.join().unwrap().unwrap();
        let waiter_policy = waiter.join().unwrap().unwrap();
        fixture.state(|state| {
            state.expire_wait = false;
            state.expire_wait_on_owner_release = false;
        });

        assert_eq!(owner_policy.hooks, waiter_policy.hooks);
        assert_eq!(
            fixture.state(|state| state.calls),
            expected_queries,
            "concurrent cold misses should run one complete Git query batch"
        );
    }

    #[test]
    fn git_policy_cache_owner_panic_releases_waiting_callers() {
        let fixture = Fixture::new();
        let key = git_policy_cache_key(&fixture.root, &git_policy_environment(&fixture.root), true)
            .unwrap();
        git_policy_cache().lock().unwrap().remove(&key);

        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        fixture.state(|state| {
            state.uncached_gate = Some(GitPolicyTestUncachedGate {
                entered: entered_tx,
                release: release_rx,
            })
        });
        let owner_root = fixture.root.clone();
        let owner =
            std::thread::spawn(move || with_worker_jail(true, || resolve_git_policy(&owner_root)));
        entered_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("miss owner did not enter uncached resolution");

        let (waiter_signal_tx, waiter_signal_rx) = std::sync::mpsc::channel();
        fixture.state(|state| state.waiter_signal = Some(waiter_signal_tx));
        let (waiter_result_tx, waiter_result_rx) = std::sync::mpsc::channel();
        let waiter_root = fixture.root.clone();
        let waiter = std::thread::spawn(move || {
            let result = with_worker_jail(true, || resolve_git_policy(&waiter_root))
                .map(|policy| policy.hooks);
            let _ = waiter_result_tx.send(result);
        });
        waiter_signal_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("same-key caller did not wait for the miss owner");

        release_tx.send(true).unwrap();
        assert!(
            owner.join().is_err(),
            "test hook should panic the miss owner"
        );
        let completed_before_cleanup =
            waiter_result_rx.recv_timeout(GIT_POLICY_FLIGHT_WAIT_TIMEOUT + Duration::from_secs(3));
        let stranded = git_policy_cache().lock().unwrap().in_flight.contains(&key);
        if stranded {
            git_policy_cache().lock().unwrap().in_flight.remove(&key);
            git_policy_cache_changed().notify_all();
        }
        let waiter_result = completed_before_cleanup
            .or_else(|_| waiter_result_rx.recv_timeout(Duration::from_secs(3)))
            .expect("waiter did not finish after flight cleanup")
            .expect("waiter policy resolution failed");
        waiter.join().unwrap();

        assert!(!stranded, "panicked owner left a permanent in-flight key");
        assert!(!waiter_result.is_empty());
    }

    #[test]
    fn git_policy_cache_waiter_resolves_uncached_after_owner_stalls() {
        let fixture = Fixture::new();
        let expected_queries = fixture.resolve().1;
        assert!(expected_queries > 0);
        let key = git_policy_cache_key(&fixture.root, &git_policy_environment(&fixture.root), true)
            .unwrap();
        git_policy_cache().lock().unwrap().remove(&key);
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        fixture.state(|state| {
            state.calls = 0;
            state.uncached_gate = Some(GitPolicyTestUncachedGate {
                entered: entered_tx,
                release: release_rx,
            })
        });
        let owner_root = fixture.root.clone();
        let owner =
            std::thread::spawn(move || with_worker_jail(true, || resolve_git_policy(&owner_root)));
        entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("miss owner did not enter uncached resolution");

        fixture.state(|state| state.expire_wait = true);
        let (waiter_result_tx, waiter_result_rx) = std::sync::mpsc::channel();
        let waiter_root = fixture.root.clone();
        let waiter = std::thread::spawn(move || {
            let result = with_worker_jail(true, || resolve_git_policy(&waiter_root));
            let _ = waiter_result_tx.send(result);
        });
        let completed_before_release = waiter_result_rx.recv_timeout(Duration::from_secs(5));
        let waiter_finished_without_owner = completed_before_release.is_ok();
        let waiter_queries = fixture.state(|state| state.calls);
        fixture.state(|state| state.expire_wait = false);
        release_tx.send(false).unwrap();
        let owner_policy = owner.join().unwrap().expect("miss owner resolution failed");
        let waiter_result = completed_before_release
            .or_else(|_| waiter_result_rx.recv_timeout(Duration::from_secs(5)))
            .expect("waiter did not finish after owner release");
        waiter.join().unwrap();

        assert!(
            waiter_finished_without_owner,
            "waiter must resolve uncached before a stalled owner is released"
        );
        let waiter_policy = waiter_result.expect("waiter policy resolution failed");
        assert_eq!(waiter_policy.hooks, owner_policy.hooks);
        assert_eq!(
            waiter_queries, expected_queries,
            "the waiter must complete its own query batch while the owner is held"
        );
        assert_eq!(fixture.resolve().1, 0, "the owner should still publish");
    }

    /// The simulated-expiry tests skip the condvar wait. This one keeps the
    /// real deadline: a waiter behind a stalled owner must neither bypass it at
    /// once nor wait past its own deadline.
    #[test]
    fn git_policy_cache_waiter_honours_its_real_wait_deadline() {
        let fixture = Fixture::new();
        let key = git_policy_cache_key(&fixture.root, &git_policy_environment(&fixture.root), true)
            .unwrap();
        git_policy_cache().lock().unwrap().remove(&key);
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        fixture.state(|state| {
            state.uncached_gate = Some(GitPolicyTestUncachedGate {
                entered: entered_tx,
                release: release_rx,
            })
        });
        let owner_root = fixture.root.clone();
        let owner =
            std::thread::spawn(move || with_worker_jail(true, || resolve_git_policy(&owner_root)));
        entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("miss owner did not enter uncached resolution");

        let (waiter_result_tx, waiter_result_rx) = std::sync::mpsc::channel();
        let waiter_root = fixture.root.clone();
        let waiter = std::thread::spawn(move || {
            let started = Instant::now();
            let result = with_worker_jail(true, || resolve_git_policy(&waiter_root));
            let _ = waiter_result_tx.send((result, started.elapsed()));
        });
        // Generous hang guard: a waiter that ignores its deadline never returns
        // while the owner is held.
        let waited = waiter_result_rx.recv_timeout(Duration::from_secs(60));
        release_tx.send(false).unwrap();
        owner.join().unwrap().expect("miss owner resolution failed");
        waiter.join().unwrap();

        let (result, elapsed) =
            waited.expect("waiter did not stop waiting at its deadline while the owner was held");
        result.expect("waiter policy resolution failed");
        // The deadline is taken inside the call, so on a monotonic clock a
        // waiter that really waited cannot return sooner than the budget.
        assert!(
            elapsed >= GIT_POLICY_FLIGHT_WAIT_TIMEOUT,
            "waiter bypassed the in-flight owner after {elapsed:?}, before its {GIT_POLICY_FLIGHT_WAIT_TIMEOUT:?} deadline"
        );
    }

    #[test]
    fn git_policy_cache_uncached_owner_hands_off_to_next_flight() {
        let fixture = Fixture::new();
        let key = git_policy_cache_key(&fixture.root, &git_policy_environment(&fixture.root), true)
            .unwrap();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (waiter_signal_tx, waiter_signal_rx) = std::sync::mpsc::channel();
        let config = fixture.root.join(".git/config");
        fixture.state(|state| {
            state.uncached_gate = Some(GitPolicyTestUncachedGate {
                entered: entered_tx,
                release: release_rx,
            });
            state.waiter_signal = Some(waiter_signal_tx);
            state.after_config_listing = Some(Box::new(move || {
                std::fs::write(config, "[core]\n hooksPath = changed-hooks\n").unwrap();
            }));
        });
        let owner_root = fixture.root.clone();
        let owner =
            std::thread::spawn(move || with_worker_jail(true, || resolve_git_policy(&owner_root)));
        entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("miss owner did not enter uncached resolution");
        let waiter_root = fixture.root.clone();
        let waiter =
            std::thread::spawn(move || with_worker_jail(true, || resolve_git_policy(&waiter_root)));
        waiter_signal_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("same-key caller did not wait for the miss owner");

        // The owner's config edit prevents publication. Even after its wait
        // budget expires, the next caller must acquire a flight before querying.
        let (next_entered_tx, next_entered_rx) = std::sync::mpsc::channel();
        let (next_release_tx, next_release_rx) = std::sync::mpsc::channel();
        fixture.state(|state| {
            state.expire_wait_on_owner_release = true;
            state.uncached_gate = Some(GitPolicyTestUncachedGate {
                entered: next_entered_tx,
                release: next_release_rx,
            });
        });
        release_tx.send(false).unwrap();
        let owner_policy = owner.join().unwrap().unwrap();
        next_entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("waiter did not take over after uncached resolution");
        let owns_flight = git_policy_cache().lock().unwrap().in_flight.contains(&key);
        next_release_tx.send(false).unwrap();
        let waiter_policy = waiter.join().unwrap().unwrap();
        fixture.state(|state| {
            state.expire_wait = false;
            state.expire_wait_on_owner_release = false;
        });

        assert!(owns_flight, "the next query batch must have a flight owner");
        assert_eq!(owner_policy.hooks, vec![fixture.root.join("changed-hooks")]);
        assert_eq!(owner_policy.hooks, waiter_policy.hooks);
        assert_eq!(fixture.resolve().1, 0, "the next owner should publish");
    }

    #[test]
    fn git_policy_cache_detects_hooks_symlink_retargeting() {
        let fixture = Fixture::new();
        fixture.resolve();
        std::fs::create_dir(fixture.root.join("redirected-hooks")).unwrap();
        std::fs::remove_dir(fixture.root.join("first-hooks")).unwrap();
        std::os::unix::fs::symlink("redirected-hooks", fixture.root.join("first-hooks")).unwrap();
        let (policy, calls) = fixture.resolve();
        assert!(calls > 0);
        assert_eq!(policy.hooks, vec![fixture.root.join("redirected-hooks")]);
    }

    #[test]
    fn git_policy_cache_pointer_and_commondir_invalidate() {
        let fixture = Fixture::new();
        let first = fixture.root.join("first-git");
        std::fs::rename(fixture.root.join(".git"), &first).unwrap();
        std::fs::write(fixture.root.join(".git"), "gitdir: first-git\n").unwrap();
        fixture.resolve();
        assert_eq!(fixture.resolve().1, 0);
        let other = fixture.root.join("other");
        std::fs::create_dir(&other).unwrap();
        assert!(git_policy_output(&other, &["init", "--quiet"])
            .unwrap()
            .status
            .success());
        std::fs::write(
            other.join(".git/config"),
            "[core]\n bare = false\n hooksPath = other-hooks\n",
        )
        .unwrap();
        std::fs::write(fixture.root.join(".git"), "gitdir: other/.git\n").unwrap();
        let (policy, calls) = fixture.resolve();
        assert!(calls > 0);
        assert_eq!(policy.hooks, vec![fixture.root.join("other-hooks")]);
        std::fs::write(fixture.root.join(".git"), "gitdir: first-git\n").unwrap();
        fixture.resolve();
        assert_eq!(fixture.resolve().1, 0);
        std::fs::write(first.join("commondir"), "../other/.git\n").unwrap();
        let (policy, calls) = fixture.resolve();
        assert!(calls > 0);
        assert_eq!(policy.hooks, vec![fixture.root.join("other-hooks")]);
        assert_eq!(fixture.resolve().1, 0);
    }

    #[test]
    fn git_policy_cache_tracks_inactive_include_candidates() {
        let fixture = Fixture::new();
        let config = fixture.root.join(".git/config");
        let mut contents = std::fs::read_to_string(&config).unwrap();
        contents
            .push_str("[includeIf \"gitdir:/nonexistent-aft-repo/\"]\n path = absent-include\n");
        std::fs::write(config, contents).unwrap();
        assert!(fixture.resolve().1 > 0);
        assert_eq!(fixture.resolve().1, 0);
        std::fs::write(
            fixture.root.join(".git/absent-include"),
            "[core]\n hooksPath = never-active-hooks\n",
        )
        .unwrap();
        assert!(
            fixture.resolve().1 > 0,
            "creating include candidate must invalidate"
        );
    }

    #[test]
    fn git_policy_cache_redirected_git_directory_is_never_cached() {
        let fixture = Fixture::new();
        fixture.state(|state| {
            state
                .environment
                .push(("GIT_DIR".into(), fixture.root.join(".git").into_os_string()))
        });
        assert!(fixture.resolve().1 > 0);
        assert!(
            fixture.resolve().1 > 0,
            "redirected Git config inputs are not tracked"
        );
    }

    /// Compare the old explicit-PATH fork with inherited-PATH helper launches.
    /// This measures jail policy preparation without entering a nested sandbox.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "manual large-heap spawn measurement; run alone with --nocapture"]
    fn large_heap_spawn_latency() {
        crate::effective_path::initialize_process_path();
        let fixture = Fixture::new();
        assert!(git_policy_inputs(
            &fixture.root,
            &fixture.root.join(".git"),
            &fixture.root.join(".git"),
            git_policy_environment(&fixture.root)
        )
        .is_some());
        let commit = git_policy_output(
            &fixture.root,
            &[
                "-c",
                "user.name=Bench",
                "-c",
                "user.email=bench@example.invalid",
                "-c",
                "core.hooksPath=/dev/null",
                "commit",
                "--quiet",
                "--allow-empty",
                "-m",
                "benchmark",
            ],
        )
        .unwrap();
        assert!(
            commit.status.success(),
            "{}",
            String::from_utf8_lossy(&commit.stderr)
        );
        let mib: usize = std::env::var("AFT_SPAWN_BENCH_HEAP_MIB")
            .ok()
            .map(|value| value.parse().unwrap())
            .unwrap_or(2048);
        // Touch real allocations, and park threads rather than generating load.
        let heap: Vec<Vec<u8>> = (0..mib * 8).map(|_| vec![1u8; 128 * 1024]).collect();
        let barrier = Arc::new(std::sync::Barrier::new(65));
        let threads: Vec<_> = (0..64)
            .map(|_| {
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let allocation = vec![7u8; 128 * 1024];
                    barrier.wait();
                    barrier.wait();
                    std::hint::black_box(allocation);
                })
            })
            .collect();
        barrier.wait();
        fn measure(name: &str, mut run: impl FnMut()) {
            let mut samples = Vec::new();
            for _ in 0..20 {
                let started = Instant::now();
                run();
                samples.push(started.elapsed().as_secs_f64() * 1000.0);
            }
            samples.sort_by(f64::total_cmp);
            eprintln!(
                "{name}: n=20 mean_ms={:.3} median_ms={:.3} p95_ms={:.3}",
                samples.iter().sum::<f64>() / 20.0,
                samples[10],
                samples[18]
            );
        }
        eprintln!(
            "heap_mib={mib} parked_threads=64 git={}",
            String::from_utf8_lossy(
                &git_policy_output(&fixture.root, &["--version"])
                    .unwrap()
                    .stdout
            )
            .trim()
        );
        for old in [true, false] {
            GIT_POLICY_BENCH_OLD.with(|value| value.set(old));
            let label = if old {
                "before_fork"
            } else {
                "after_posix_spawn"
            };
            measure(&format!("worktree_probe/{label}"), || {
                assert!(git_policy_output(
                    &fixture.root,
                    &[
                        "rev-parse",
                        "--path-format=absolute",
                        "--git-dir",
                        "--git-common-dir"
                    ]
                )
                .unwrap()
                .status
                .success());
            });
            measure(&format!("cache_key_resolve/{label}"), || {
                assert!(
                    git_policy_output(&fixture.root, &["rev-list", "--max-parents=0", "HEAD"])
                        .unwrap()
                        .status
                        .success()
                );
            });
            measure(&format!("jail_policy_cold/{label}"), || {
                if let Some(key) = git_policy_cache_key(
                    &fixture.root,
                    &git_policy_environment(&fixture.root),
                    true,
                ) {
                    git_policy_cache().lock().unwrap().remove(&key);
                }
                fixture.resolve();
            });
            fixture.resolve();
            measure(&format!("jail_policy_warm/{label}"), || {
                fixture.resolve();
            });
            measure(&format!("shell_preexec/{label}"), || {
                use std::os::fd::AsRawFd;
                let marker = File::open("/dev/null").unwrap();
                let mut command = crate::effective_path::new_command("/bin/sh");
                if old {
                    command.env("PATH", crate::effective_path::effective_path());
                }
                let mut command = command.build();
                command.args(["-c", "true"]);
                crate::bash_background::process::start_new_session(&mut command);
                apply_marker_fd_allowlist(&mut command, marker.as_raw_fd(), marker.as_raw_fd())
                    .unwrap();
                crate::privacy_spawn::install(&mut command, true, false).unwrap();
                assert!(command.status().unwrap().success());
            });
        }
        GIT_POLICY_BENCH_OLD.with(|value| value.set(false));
        barrier.wait();
        for thread in threads {
            thread.join().unwrap();
        }
        std::hint::black_box(heap);
    }
}

#[cfg(all(test, unix))]
#[path = "sandbox_scope_grants_tests.rs"]
mod scope_grant_tests;

#[cfg(test)]
mod policy_tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn sandbox_task_temp_supports_long_unix_socket_names() {
        use std::os::unix::net::{UnixListener, UnixStream};
        let root = tempfile::tempdir().unwrap();
        let bundle = root
            .path()
            .join("runner/bash-tasks/0123456789abcdef/bash-0123456789abcdef/io");
        std::fs::create_dir_all(&bundle).unwrap();
        let temp = create_task_temp_dir(&bundle).unwrap();
        let nested = temp.join("a".repeat(30));
        std::fs::create_dir(&nested).unwrap();
        let socket = nested.join(format!("{}.sock", "b".repeat(30)));
        let bound = UnixListener::bind(&socket);
        if bound.is_ok() {
            UnixStream::connect(&socket).unwrap();
        }
        cleanup_task_temp_dir(&temp).unwrap();
        assert!(bound.is_ok(), "cannot bind {}: {bound:?}", socket.display());
    }

    #[cfg(unix)]
    #[test]
    fn worker_build_slot_grants_are_exact_and_reject_model_writable_launchers() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        let store = home.join(".local/share/cortexkit");
        let project = store.join("alfonso/worktrees/container/own");
        let slots = store.join("operator/build-slots");
        for path in [&project, &slots] {
            std::fs::create_dir_all(path).unwrap();
        }
        for name in [
            "prefrontal-build-slot",
            "rustc-lowprio-direct",
            ".root",
            ".cache",
        ] {
            std::fs::write(slots.join(name), "operator-owned").unwrap();
        }
        std::fs::write(slots.join("limit"), "2\n").unwrap();
        let policy = crate::access_policy::AccessPolicy::from_config(
            &crate::config::Config::default(),
            &store.join("aft"),
            vec![project.clone()],
            &home,
        );
        let mut grants = WorkerJailGrants::default();
        add_build_slot_grants(
            &slots.join("prefrontal-build-slot"),
            &[project.clone()],
            &policy,
            &mut grants,
        );
        assert_eq!(
            grants.files,
            ["queue.lock", "0.lock", "1.lock", "2.lock"].map(|name| slots.join(name))
        );
        assert_eq!(grants.read_only.len(), 5);
        assert!(grants.roots.is_empty() && grants.caches.is_empty());
        assert!(grants
            .read_only
            .iter()
            .all(|path| path.is_file() && path.parent() == Some(slots.as_path())));
        let mut refused = WorkerJailGrants::default();
        add_build_slot_grants(
            &slots.join("prefrontal-build-slot"),
            &[slots.clone()],
            &policy,
            &mut refused,
        );
        assert_eq!(
            refused,
            WorkerJailGrants::default(),
            "a model-writable launcher must never be pinned"
        );
        #[cfg(unix)]
        {
            std::fs::remove_file(slots.join(".root")).unwrap();
            std::fs::write(home.join("secret"), "secret").unwrap();
            std::os::unix::fs::symlink(home.join("secret"), slots.join(".root")).unwrap();
            add_build_slot_grants(
                &slots.join("prefrontal-build-slot"),
                &[project],
                &policy,
                &mut refused,
            );
            assert_eq!(
                refused,
                WorkerJailGrants::default(),
                "a sibling symlink must not widen the read exception"
            );
        }
    }

    fn context(project_root: PathBuf) -> AppContext {
        AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                project_root: Some(project_root),
                sandbox: crate::config::SandboxConfig {
                    profile: crate::config::SandboxRoles {
                        head: SandboxMode::Strict,
                        worker: SandboxMode::Strict,
                    },
                    ..crate::config::SandboxConfig::default()
                },
                ..crate::config::Config::default()
            },
        )
    }

    #[test]
    fn windows_relaxed_profile_runs_unenforced_but_strict_refuses() {
        assert_eq!(
            platform_sandbox_plan(SandboxMode::Relaxed, RequestedSandboxTier::Native, false),
            Some(SpawnPlan::RelaxedUnenforced)
        );
        assert_eq!(
            platform_sandbox_plan(SandboxMode::Off, RequestedSandboxTier::Native, false),
            Some(SpawnPlan::Unsandboxed)
        );
        let strict =
            platform_sandbox_plan(SandboxMode::Strict, RequestedSandboxTier::Native, false)
                .unwrap();
        assert_eq!(strict.refusal_code(), Some("sandbox_unavailable"));
        assert_eq!(strict.refusal_message(), Some("sandbox profile strict is not enforced on Windows; select sandbox.profile off or run on macOS/Linux"));
        for tier in [RequestedSandboxTier::Disabled, RequestedSandboxTier::Host] {
            assert!(platform_sandbox_plan(SandboxMode::Relaxed, tier, false)
                .unwrap()
                .refusal_code()
                .is_some());
        }
        assert_eq!(
            platform_sandbox_plan(SandboxMode::Relaxed, RequestedSandboxTier::Native, true),
            None
        );
    }

    #[cfg(unix)]
    fn normalized_tool_profile(profile: &SandboxProfile) -> serde_json::Value {
        fn normalize(value: &mut serde_json::Value, temp: &str) {
            match value {
                serde_json::Value::String(text) => *text = text.replace(temp, "<private-temp>"),
                serde_json::Value::Array(values) => {
                    values.iter_mut().for_each(|value| normalize(value, temp))
                }
                serde_json::Value::Object(values) => {
                    values.values_mut().for_each(|value| normalize(value, temp))
                }
                _ => {}
            }
        }
        let mut value = serde_json::to_value(profile).unwrap();
        normalize(&mut value, &profile.temp_dir.to_string_lossy());
        value
    }

    #[cfg(unix)]
    enum PolicyParityPhase {
        BeforeTool,
        BetweenBuilds,
    }

    #[cfg(unix)]
    fn assert_tool_shell_policy_parity(mut hook: impl FnMut(&Path, PolicyParityPhase)) {
        #[cfg(target_os = "linux")]
        let root = tempfile::Builder::new()
            .prefix("aft-policy-parity-")
            .tempdir_in(absolute_env_path("HOME").expect("profile tests require an absolute HOME"))
            .unwrap();
        #[cfg(not(target_os = "linux"))]
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let account = root.path().join("account");
        std::fs::create_dir(&account).unwrap();
        std::fs::create_dir(root.path().join("store")).unwrap();
        let cache_marker = root.path().join("fixed-cache-marker");
        std::fs::create_dir(&cache_marker).unwrap();
        let cache_marker = cache_marker.canonicalize().unwrap();
        #[cfg(target_os = "linux")]
        let marker = root.path().join("fixed-inventory-marker");
        #[cfg(target_os = "linux")]
        std::fs::create_dir(&marker).unwrap();
        #[cfg(target_os = "linux")]
        let marker = marker.canonicalize().unwrap();
        let ctx = context(project.clone());
        let operator_binary = root.path().join("operator-aft");
        std::fs::write(&operator_binary, "operator executable fixture").unwrap();
        let _launcher = install_sandbox_launcher_for_test(&project, operator_binary.clone());
        ctx.update_config(|config| {
            config.storage_dir = Some(root.path().join("store"));
            config.gh_shim.binary_path = Some(operator_binary);
        });
        let untrusted = AuthenticatedPrincipal::RouteBind {
            trust: PrincipalTrust::Untrusted,
            route_channel: 7,
            route_epoch: 1,
            project_root: project,
            harness: "mcp:test".into(),
            session_id: "tool-policy".into(),
            principal_id: Some("unverified".into()),
        };
        let _candidates = candidate_snapshot::install(root.path());
        #[cfg(target_os = "linux")]
        let _inventory = fixed_read_inventory::install(root.path());
        for jailed in [false, true] {
            for mode in [SandboxMode::Off, SandboxMode::Relaxed, SandboxMode::Strict] {
                for principal in [AuthenticatedPrincipal::FirstParty, untrusted.clone()] {
                    ctx.update_config(|config| {
                        config.sandbox.profile.head =
                            if jailed { SandboxMode::Strict } else { mode };
                        config.sandbox.profile.worker =
                            if jailed { mode } else { SandboxMode::Off };
                    });
                    hook(root.path(), PolicyParityPhase::BeforeTool);
                    let reads = if principal_requires_strict(&principal)
                        || mode == SandboxMode::Strict
                        || (mode == SandboxMode::Relaxed && (!jailed || !cfg!(target_os = "linux")))
                    {
                        2
                    } else {
                        0
                    };
                    crate::worker_jail_canary::with_test_preparation_home(&account, reads, || {
                        with_authenticated_principal(principal.clone(), || {
                            with_worker_jail(jailed, || {
                                let policy = ToolLaunchPolicy::resolve(&ctx);
                                let io = policy
                                    .task
                                    .as_ref()
                                    .map(|task| task.paths.io_dir.as_path())
                                    .unwrap_or(Path::new(""));
                                hook(root.path(), PolicyParityPhase::BetweenBuilds);
                                let shell = resolve_sandbox_spawn(
                                    &ctx,
                                    &principal,
                                    RequestedSandboxTier::Native,
                                    SandboxTaskKind::BashForeground,
                                    io,
                                    None,
                                );
                                match (&policy.plan, &shell) {
                                    (SpawnPlan::Unsandboxed, SpawnPlan::Unsandboxed) => {
                                        assert_eq!(mode, SandboxMode::Off);
                                        assert!(!jailed);
                                        assert!(!principal_requires_strict(&principal));
                                    }
                                    (
                                        SpawnPlan::Launcher { profile: tool, .. },
                                        SpawnPlan::Launcher { profile: bash, .. },
                                    ) => {
                                        let expected = if principal_requires_strict(&principal) {
                                            SandboxMode::Strict
                                        } else {
                                            mode
                                        };
                                        assert_eq!(tool.mode, expected);
                                        if expected == SandboxMode::Strict {
                                            assert!(tool.cache_roots.contains(&cache_marker));
                                            assert!(bash.cache_roots.contains(&cache_marker));
                                        }
                                        #[cfg(target_os = "linux")]
                                        if expected == SandboxMode::Strict {
                                            assert!(tool.read_allow.contains(&marker));
                                            assert!(bash.read_allow.contains(&marker));
                                        }
                                        assert_eq!(
                                            normalized_tool_profile(tool),
                                            normalized_tool_profile(bash)
                                        );
                                    }
                                    other => panic!(
                                    "tool and shell must resolve the same launch policy: {other:?}"
                                ),
                                }
                                shell.cleanup_unspawned();
                            })
                        })
                    });
                }
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn tool_policy_matches_session_shell_for_every_role_and_untrusted_bind() {
        assert_tool_shell_policy_parity(|_, _| {});
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn tool_policy_matches_session_shell_despite_directory_churn_between_builds() {
        let mut changes = 0;
        assert_tool_shell_policy_parity(|root, phase| {
            let before = root.join("inventory-before");
            let after = root.join("inventory-after");
            match phase {
                PolicyParityPhase::BeforeTool => {
                    if after.exists() {
                        std::fs::remove_dir(&after).unwrap();
                    }
                    std::fs::create_dir(&before).unwrap();
                }
                PolicyParityPhase::BetweenBuilds => {
                    std::fs::remove_dir(&before).unwrap();
                    std::fs::create_dir(&after).unwrap();
                    changes += 1;
                }
            }
        });
        assert_eq!(
            changes, 12,
            "the hook must run between every pair of builds"
        );
    }

    #[cfg(unix)]
    #[test]
    fn tool_policy_matches_session_shell_despite_cache_candidate_creation_between_builds() {
        let mut changes = 0;
        assert_tool_shell_policy_parity(|root, phase| {
            let candidate = root.join(".cargo/git");
            match phase {
                PolicyParityPhase::BeforeTool => {
                    if candidate.exists() {
                        std::fs::remove_dir_all(&candidate).unwrap();
                    }
                }
                PolicyParityPhase::BetweenBuilds => {
                    std::fs::create_dir_all(&candidate).unwrap();
                    assert!(candidate.is_dir());
                    changes += 1;
                }
            }
        });
        assert_eq!(changes, 12);
    }

    #[cfg(unix)]
    #[test]
    fn tool_policy_filters_environment_and_cleans_private_task_directory() {
        assert!(
            std::env::var_os("CARGO_MANIFEST_DIR").is_some(),
            "cargo must supply the daemon-only environment canary"
        );
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let ctx = context(project);
        ctx.update_config(|config| config.storage_dir = Some(root.path().join("store")));
        let launcher = root.path().join("launcher");
        std::fs::write(&launcher, "#!/bin/sh\nshift 4\nexec \"$@\"\n").unwrap();
        std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut policy = ToolLaunchPolicy::resolve(&ctx);
        let temp = match &mut policy.plan {
            SpawnPlan::Launcher {
                profile,
                launcher_path,
            } => {
                *launcher_path = launcher;
                profile.temp_dir.clone()
            }
            other => panic!("strict tools need a launcher: {other:?}"),
        };
        let io = policy.task.as_ref().unwrap().paths.io_dir.clone();
        for capture in [false, true] {
            let result = if capture {
                crate::format::run_external_tool_capture(
                    &policy,
                    "/bin/sh",
                    &["-c", "env; exit 3"],
                    None,
                    10,
                )
            } else {
                crate::format::run_external_tool(&policy, "/bin/sh", &["-c", "env"], None, 10)
            }
            .unwrap();
            assert_eq!(result.exit_code, if capture { 3 } else { 0 });
            assert!(
                !result
                    .stdout
                    .lines()
                    .any(|line| line.starts_with("CARGO_MANIFEST_DIR=")),
                "{}",
                result.stdout
            );
            assert!(
                result
                    .stdout
                    .lines()
                    .any(|line| line == format!("TMPDIR={}", temp.display())),
                "{}",
                result.stdout
            );
        }
        assert!(io.exists() && temp.exists());
        drop(policy);
        assert!(!io.exists() && !temp.exists());
    }

    #[test]
    fn tool_policy_refuses_host_escalation() {
        let mut policy = ToolLaunchPolicy::unconfined_for_test();
        policy.plan = SpawnPlan::Host {
            shell_path: PathBuf::from("sh"),
            environment: BTreeMap::new(),
        };
        assert!(policy
            .command(OsStr::new("formatter"), &[])
            .unwrap_err()
            .contains("host escalation"));
    }

    #[cfg(unix)]
    fn assert_scoped_profile_grants(profile: &SandboxProfile, write: &Path, read: &Path) {
        assert!(
            profile
                .writable_roots
                .contains(&write.canonicalize().unwrap()),
            "{:?}",
            profile.writable_roots
        );
        // Seatbelt uses these read exceptions directly; Landlock also lowers
        // them into its top-level allowlist during Linux profile construction.
        assert!(
            profile
                .data_policy
                .read_allow
                .contains(&read.canonicalize().unwrap()),
            "{:?}",
            profile.data_policy.read_allow
        );
    }

    #[cfg(unix)]
    fn assert_scoped_tool_grant_fixture(root: &Path, persist_aliases: bool) {
        use crate::access_policy::{AccessGrant, AccessOperation, GrantKind};
        use crate::db::access_grants::{self, ScopeKey};
        let project = root.join("project");
        let store = root.join("store");
        let write = root.join("approved-write");
        let read = root.join("approved-read");
        let common_temp = root.join("common-temp");
        for path in [&project, &store, &write, &read, &common_temp] {
            std::fs::create_dir(path).unwrap();
        }
        let ctx = context(project);
        ctx.update_config(|config| config.storage_dir = Some(store.clone()));
        let scope = ScopeKey {
            owner: "reserved:prefrontal-core".into(),
            scope_ref: "tool-policy".into(),
            epoch: 1,
        };
        let conn = crate::db::open(&store.join("aft.db")).unwrap();
        access_grants::register_scope(&conn, &scope, None, &ctx.config()).unwrap();
        for (path, operation) in [
            (&write, AccessOperation::Write),
            (&read, AccessOperation::Read),
        ] {
            access_grants::insert(
                &conn,
                &scope,
                &AccessGrant {
                    path: path.clone(),
                    operation,
                    kind: GrantKind::Subtree,
                },
            )
            .unwrap();
            if persist_aliases {
                // Keep the alias in storage so replay, not just insertion,
                // must resolve the persisted path to the same directory.
                assert_ne!(*path, path.canonicalize().unwrap());
                assert_eq!(conn.execute(
                    "UPDATE access_grants SET path=?1 WHERE owner=?2 AND scope_ref=?3 AND epoch=?4 AND path=?5",
                    rusqlite::params![path.to_string_lossy(), scope.owner, scope.scope_ref, scope.epoch.to_string(), path.canonicalize().unwrap().to_string_lossy()],
                ).unwrap(), 1);
            }
        }
        if persist_aliases {
            let stored = access_grants::list(&conn, &scope).unwrap();
            assert!(stored.iter().any(|grant| grant.path == write));
            assert!(stored.iter().any(|grant| grant.path == read));
        }
        ctx.set_db(Arc::new(std::sync::Mutex::new(conn)));
        let policy = crate::access_policy::with_scope(Some(scope.clone()), || {
            ToolLaunchPolicy::resolve(&ctx)
        });
        let SpawnPlan::Launcher { profile, .. } = &policy.plan else {
            panic!(
                "a scoped strict tool must get its shell profile: {:?}",
                policy.plan
            );
        };
        assert_scoped_profile_grants(profile, &write, &read);
        #[cfg(target_os = "linux")]
        assert!(
            profile.read_allow.contains(&read.canonicalize().unwrap()),
            "{:?}",
            profile.read_allow
        );
        if persist_aliases {
            // Seatbelt consumes the common profile without Landlock's lowering.
            let common = crate::access_policy::with_scope(Some(scope), || {
                ctx.access_policy("")
                    .unwrap()
                    .compile_shell(common_temp, Vec::new(), Vec::new())
                    .unwrap()
                    .canonicalize_for_launch()
                    .unwrap()
            });
            assert!(common.read_allow.is_empty());
            let transferred: SandboxProfile =
                serde_json::from_slice(&serde_json::to_vec(&common).unwrap()).unwrap();
            assert_scoped_profile_grants(
                &transferred.canonicalize_for_launch().unwrap(),
                &write,
                &read,
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn tool_policy_keeps_session_grants_that_config_alone_cannot_recover() {
        let root = tempfile::tempdir().unwrap();
        assert_scoped_tool_grant_fixture(root.path(), false);
    }

    #[cfg(unix)]
    #[test]
    fn tool_policy_keeps_session_grants_stored_under_var_folders_alias() {
        // Reproduce /var -> /private/var beneath a fixture root on every Unix host.
        let root = tempfile::tempdir().unwrap();
        let canonical_root = root.path().canonicalize().unwrap();
        let real = canonical_root.join("private/var/folders");
        std::fs::create_dir_all(&real).unwrap();
        std::os::unix::fs::symlink(
            canonical_root.join("private/var"),
            canonical_root.join("var"),
        )
        .unwrap();
        let alias = canonical_root.join("var/folders");
        assert_eq!(alias.canonicalize().unwrap(), real);
        assert_scoped_tool_grant_fixture(&alias, true);
    }

    #[test]
    fn sandbox_profile_roles_and_unverified_routes_resolve_before_spawning() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path().to_path_buf());
        ctx.update_config(|config| config.sandbox = crate::config::SandboxConfig::default());
        assert_eq!(
            effective_sandbox_mode(&ctx, &AuthenticatedPrincipal::FirstParty),
            SandboxMode::Off
        );
        with_worker_jail(true, || {
            assert_eq!(
                effective_sandbox_mode(&ctx, &AuthenticatedPrincipal::FirstParty),
                SandboxMode::Relaxed
            )
        });
        ctx.update_config(|config| config.sandbox.profile.worker = SandboxMode::Off);
        with_worker_jail(true, || {
            assert_eq!(
                effective_sandbox_mode(&ctx, &AuthenticatedPrincipal::FirstParty),
                SandboxMode::Off
            )
        });
        for (trust, harness, principal_id) in [
            (PrincipalTrust::Untrusted, "runner", Some("direct")),
            (PrincipalTrust::Untrusted, "mcp:test", Some("direct")),
            (PrincipalTrust::FirstParty, "mcp:test", Some("direct")),
            (PrincipalTrust::FirstParty, "runner", Some("unverified")),
            (PrincipalTrust::FirstParty, "runner", None),
        ] {
            let principal = AuthenticatedPrincipal::RouteBind {
                trust,
                route_channel: 7,
                route_epoch: 1,
                project_root: dir.path().to_path_buf(),
                harness: harness.into(),
                session_id: "role-test".into(),
                principal_id: principal_id.map(str::to_string),
            };
            assert_eq!(
                effective_sandbox_mode(&ctx, &principal),
                SandboxMode::Strict,
                "{principal:?}"
            );
            with_worker_jail(true, || {
                assert_eq!(
                    effective_sandbox_mode(&ctx, &principal),
                    SandboxMode::Strict
                )
            });
        }
    }

    #[test]
    fn trusted_direct_non_mcp_harness_preserves_role_profiles() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path().to_path_buf());
        ctx.update_config(|config| config.sandbox = crate::config::SandboxConfig::default());
        let principal = AuthenticatedPrincipal::RouteBind {
            trust: PrincipalTrust::FirstParty,
            route_channel: 7,
            route_epoch: 1,
            project_root: dir.path().to_path_buf(),
            harness: "runner".into(),
            session_id: "trusted-direct".into(),
            principal_id: Some("direct".into()),
        };
        assert_eq!(effective_sandbox_mode(&ctx, &principal), SandboxMode::Off);
        with_worker_jail(true, || {
            assert_eq!(
                effective_sandbox_mode(&ctx, &principal),
                SandboxMode::Relaxed
            );
        });
        ctx.update_config(|config| config.sandbox.profile.head = SandboxMode::Strict);
        assert_eq!(
            effective_sandbox_mode(&ctx, &principal),
            SandboxMode::Strict
        );
    }

    #[test]
    fn first_party_mcp_harness_forces_strict_for_every_role_profile() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path().to_path_buf());
        let principal = AuthenticatedPrincipal::RouteBind {
            trust: PrincipalTrust::FirstParty,
            route_channel: 7,
            route_epoch: 1,
            project_root: dir.path().to_path_buf(),
            harness: "mcp:evil".into(),
            session_id: "first-party-mcp".into(),
            principal_id: Some("direct".into()),
        };
        for head in [SandboxMode::Off, SandboxMode::Relaxed, SandboxMode::Strict] {
            for worker in [SandboxMode::Off, SandboxMode::Relaxed, SandboxMode::Strict] {
                ctx.update_config(|config| {
                    config.sandbox.profile.head = head;
                    config.sandbox.profile.worker = worker;
                });
                for jailed in [false, true] {
                    with_worker_jail(jailed, || {
                        assert_eq!(
                            effective_sandbox_mode(&ctx, &principal),
                            SandboxMode::Strict,
                            "MCP confinement must hold for head={head}, worker={worker}, jailed={jailed}"
                        );
                    });
                }
            }
        }
    }

    #[test]
    fn relaxed_worker_environment_preserves_compile_server_settings() {
        let mut environment = HashMap::from([
            ("SCCACHE_SERVER_PORT".into(), "4321".into()),
            ("RUSTC_WRAPPER".into(), "/operator/sccache".into()),
        ]);
        with_worker_jail(true, || {
            apply_worker_jail_environment(&mut environment, SandboxMode::Relaxed)
        });
        assert_eq!(environment["SCCACHE_SERVER_PORT"], "4321");
        assert_eq!(environment["RUSTC_WRAPPER"], "/operator/sccache");
    }

    #[test]
    fn relaxed_compile_server_inheritance_excludes_cache_credentials() {
        for key in [
            "SCCACHE_SERVER_PORT",
            "SCCACHE_SERVER_UDS",
            "SCCACHE_DIR",
            "RUSTC_WRAPPER",
            "CARGO_BUILD_RUSTC_WRAPPER",
        ] {
            assert!(relaxed_compile_environment_key(key), "{key}");
        }
        for key in [
            "SCCACHE_REDIS_PASSWORD",
            "SCCACHE_GCS_OAUTH_TOKEN",
            "SCCACHE_AZURE_CONNECTION_STRING",
            "AWS_SECRET_ACCESS_KEY",
            "SUBC_CONNECTION_FILE",
        ] {
            assert!(!relaxed_compile_environment_key(key), "{key}");
        }
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
                    apply_marker_fd_allowlist(&mut command, exit.as_raw_fd(), failure.as_raw_fd())
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
    fn worker_jail_reads_only_build_settings_from_cargo_config() {
        assert_eq!(
            parse_cargo_build_settings(
                "[build]\nrustc-wrapper = \"/x/rustc-lowprio\"\ntarget-dir = \"shared-target\"\njobs = 12\n"
            ),
            Some(CargoBuildSettings {
                target_dir: Some("shared-target".into()),
                rustc_wrapper: Some("/x/rustc-lowprio".into()),
            })
        );
        assert_eq!(parse_cargo_build_settings("[net]\noffline = true\n"), None);
        assert_eq!(parse_cargo_build_settings("not toml ["), None);
    }

    #[cfg(unix)]
    #[test]
    fn worker_jail_sccache_ports_always_include_default_and_operator_port() {
        assert_eq!(sccache_server_ports(None), vec![4226]);
        assert_eq!(sccache_server_ports(Some("invalid")), vec![4226]);
        assert_eq!(sccache_server_ports(Some("65536")), vec![4226]);
        assert_eq!(sccache_server_ports(Some("0")), vec![4226]);
        assert_eq!(sccache_server_ports(Some("4226")), vec![4226]);
        assert_eq!(sccache_server_ports(Some("54226")), vec![4226, 54226]);
    }

    #[test]
    fn worker_jail_environment_scrubs_sccache_channels_only_when_jailed() {
        let keys = [
            "SCCACHE_SERVER_PORT",
            "SCCACHE_SERVER_UDS",
            "SCCACHE_STARTUP_NOTIFY",
            "SCCACHE_REDIS_ENDPOINT",
            "SCCACHE_WEBDAV_ENDPOINT",
            "SCCACHE_ENDPOINT",
            "SCCACHE_DIRECT",
            "SCCACHE_FUTURE_SERVER_CHANNEL",
        ];
        let original = keys
            .into_iter()
            .chain(["KEEP_ME"])
            .map(|key| (key.to_string(), "operator-value".to_string()))
            .collect::<HashMap<_, _>>();
        let mut environment = original.clone();
        with_worker_jail(false, || {
            apply_worker_jail_environment(&mut environment, SandboxMode::Strict)
        });
        assert_eq!(environment, original);
        with_worker_jail(true, || {
            apply_worker_jail_environment(&mut environment, SandboxMode::Strict)
        });
        for key in keys {
            assert!(
                !environment.contains_key(key),
                "jailed client retained {key}"
            );
        }
        assert_eq!(environment["KEEP_ME"], "operator-value");
    }

    #[cfg(unix)]
    #[test]
    fn worker_jail_environment_disables_rustc_wrappers_only_when_jailed() {
        let mut free = HashMap::new();
        apply_worker_jail_environment(&mut free, SandboxMode::Strict);
        assert!(free.is_empty());
        let mut jailed = HashMap::from([("RUSTC_WRAPPER".to_string(), "sccache".to_string())]);
        with_worker_jail(true, || {
            apply_worker_jail_environment(&mut jailed, SandboxMode::Strict)
        });
        assert_eq!(jailed["RUSTC_WRAPPER"], "");
        assert_eq!(jailed["CARGO_BUILD_RUSTC_WRAPPER"], "");
    }

    #[cfg(unix)]
    #[test]
    fn worker_jail_target_may_not_contain_home_roots_or_protected_paths() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let home = base.join("home");
        let root = base.join("work").join("worktree");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        let policy = crate::access_policy::AccessPolicy::from_config(
            &crate::config::Config::default(),
            &base.join("storage"),
            vec![root.clone()],
            &home,
        );
        let roots = [root.clone()];
        assert!(jail_target_allowed(
            &base.join("shared-target"),
            &home,
            &roots,
            &policy
        ));
        assert!(jail_target_allowed(
            &root.join("target"),
            &home,
            &roots,
            &policy
        ));
        assert!(!jail_target_allowed(&base, &home, &roots, &policy));
        assert!(!jail_target_allowed(&home, &home, &roots, &policy));
        assert!(!jail_target_allowed(
            &base.join("work"),
            &home,
            &roots,
            &policy
        ));
        assert!(!jail_target_allowed(
            &home.join(".ssh/target"),
            &home,
            &roots,
            &policy
        ));
        assert!(!jail_target_allowed(
            &home.join(".local/share/cortexkit/target"),
            &home,
            &roots,
            &policy
        ));
        assert!(!jail_target_allowed(
            Path::new("relative"),
            &home,
            &roots,
            &policy
        ));
        assert!(!jail_target_allowed(Path::new("/"), &home, &roots, &policy));
    }

    #[cfg(unix)]
    #[test]
    fn worker_jail_git_grants_cover_shared_stores_but_not_config_hooks_or_siblings() {
        let dir = tempfile::tempdir().unwrap();
        let common = dir.path().join("main/.git");
        let own = common.join("worktrees/own");
        for path in [
            common.join("objects"),
            common.join("refs"),
            common.join("logs"),
            common.join("hooks"),
            common.join("info"),
            own.clone(),
            common.join("worktrees/sibling"),
        ] {
            std::fs::create_dir_all(path).unwrap();
        }
        let linked = GitPolicy {
            read_roots: Vec::new(),
            hooks: vec![common.join("hooks")],
            linked: Some((own.clone(), common.clone())),
            configured_paths: Vec::new(),
            metadata_allow: Vec::new(),
        };
        let main = GitPolicy {
            read_roots: Vec::new(),
            hooks: Vec::new(),
            linked: None,
            configured_paths: Vec::new(),
            metadata_allow: Vec::new(),
        };
        let grants = linked_git_grants(&[linked, main]);
        assert_eq!(
            grants,
            vec![
                own,
                common.join("objects"),
                common.join("refs"),
                common.join("logs")
            ]
        );
        for refused in ["config", "hooks", "info", "worktrees/sibling", ""] {
            let path = common.join(refused);
            assert!(
                !grants.iter().any(|grant| path.starts_with(grant)),
                "{} must stay read-only",
                path.display()
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn worker_jail_packed_refs_grants_are_exact_files_even_before_creation() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let home = base.join("home");
        let worktree = base.join("worktree");
        let common = base.join("main/.git");
        let own = common.join("worktrees/own");
        for path in [&home, &worktree, &own] {
            std::fs::create_dir_all(path).unwrap();
        }
        let policy = crate::access_policy::AccessPolicy::from_config(
            &crate::config::Config::default(),
            &base.join("storage"),
            vec![worktree.clone()],
            &home,
        );
        let git = GitPolicy {
            read_roots: Vec::new(),
            hooks: vec![common.join("hooks")],
            linked: Some((own, common.clone())),
            configured_paths: Vec::new(),
            metadata_allow: Vec::new(),
        };
        let grants = worker_jail_grants(&home, &[worktree], &[git], &policy);
        let files = [
            common.join("packed-refs"),
            common.join("packed-refs.lock"),
            common.join("packed-refs.new"),
        ];
        for file in &files {
            assert!(!file.exists());
            assert!(
                grants.files.contains(file),
                "missing exact file grant: {file:?}"
            );
        }
        assert!(!grants.roots.iter().any(|root| common.starts_with(root)));
        for name in [
            "config",
            "hooks/post-commit",
            "info/exclude",
            "packed-refs.other",
            "packed-refs.new.other",
            "HEAD.lock",
            "ORIG_HEAD",
            "FETCH_HEAD",
        ] {
            let refused = common.join(name);
            assert!(!grants.files.contains(&refused));
            assert!(!grants.roots.iter().any(|root| refused.starts_with(root)));
        }

        // A lock can disappear between collecting private-store exceptions and
        // canonicalizing the profile, or before the launcher revalidates it.
        let mut policy = policy;
        policy.private_stores.read_allow.extend(files.clone());
        policy.private_stores.write_allow.extend(files.clone());
        let mut profile = policy
            .compile_shell(home.clone(), Vec::new(), Vec::new())
            .unwrap();
        profile.writable_roots.extend(grants.roots);
        profile.writable_files = grants.files;
        let profile = profile.canonicalize_for_launch().unwrap();
        let transferred: SandboxProfile =
            serde_json::from_slice(&serde_json::to_vec(&profile).unwrap()).unwrap();
        assert_eq!(transferred.canonicalize_for_launch().unwrap(), profile);
        for file in &files {
            assert!(profile.writable_files.contains(file));
            assert!(profile.data_policy.read_allow.contains(file));
            assert!(profile.data_policy.write_allow.contains(file));
        }
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
        assert!(message.ends_with("select sandbox.profile off to disable native sandboxing"));
    }

    #[cfg(unix)]
    #[test]
    fn native_sandbox_predicate_controls_spawn_and_rewrite() {
        let project = tempfile::tempdir().unwrap();
        let file = project.path().join("rewrite-probe.txt");
        std::fs::write(&file, "sandboxed\n").unwrap();
        let ctx = context(project.path().to_path_buf());
        ctx.update_config(|config| config.experimental_bash_rewrite = true);
        // Capture files belong to the private task store, not the project. Using
        // the project as task IO would also deny project access on Landlock.
        let task = crate::bash_background::persistence::allocate_task_layout(
            &ctx.storage_dir(),
            "sandbox-predicate-test",
        )
        .unwrap();
        let principal = AuthenticatedPrincipal::FirstParty;
        let command = format!("cat {}", file.display());

        assert!(native_sandbox_enforced(&ctx, &principal));
        assert!(crate::bash_rewrite::try_rewrite(&command, None, &ctx, &principal).is_none());
        let sandboxed = resolve_sandbox_spawn(
            &ctx,
            &principal,
            RequestedSandboxTier::Native,
            SandboxTaskKind::BashForeground,
            &task.paths.io_dir,
            None,
        );
        assert!(
            matches!(&sandboxed, SpawnPlan::Launcher { .. }),
            "native sandbox must use the launcher: {sandboxed:?}"
        );
        sandboxed.cleanup_unspawned();

        ctx.update_config(|config| config.sandbox.profile.head = crate::config::SandboxMode::Off);
        assert!(!native_sandbox_enforced(&ctx, &principal));
        assert!(crate::bash_rewrite::try_rewrite(&command, None, &ctx, &principal).is_some());
        assert_eq!(
            resolve_sandbox_spawn(
                &ctx,
                &principal,
                RequestedSandboxTier::Native,
                SandboxTaskKind::BashForeground,
                &task.paths.io_dir,
                None,
            ),
            SpawnPlan::Unsandboxed
        );
    }

    #[test]
    fn untrusted_principal_forces_strict_even_when_profiles_are_off() {
        let project = tempfile::tempdir().unwrap();
        let ctx = context(project.path().to_path_buf());
        ctx.update_config(|config| {
            config.sandbox.profile.worker = SandboxMode::Off;
            config.sandbox.profile.head = SandboxMode::Off;
        });
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
        assert_eq!(
            effective_sandbox_mode(&ctx, &principal),
            SandboxMode::Strict
        );
        assert!(
            matches!(&plan, SpawnPlan::Launcher { profile, .. } if profile.mode == SandboxMode::Strict)
                || plan.refusal_code() == Some("sandbox_unavailable"),
            "untrusted principal must be confined or refused, never downgraded: {plan:?}"
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
    fn unclaimable_escalation_payloads_are_reclaimed_and_refusals_stay_exact() {
        let project = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let ctx = context(project.path().to_path_buf());
        let principal = AuthenticatedPrincipal::FirstParty;
        let environment =
            ChildEnvironment::from([(OsString::from("ONLY"), OsString::from("snapshot"))]);
        let bundle_exists = |grant_id: &str| {
            let store = ctx.escalation_grants().lock();
            let grant = &store.grants[grant_id];
            grant.session_dir.join(&grant.task_id).exists()
                || grant
                    .session_dir
                    .join(format!("{}.json", grant.task_id))
                    .exists()
        };
        let start = Instant::now();
        let stale = mint_test_grant(
            &ctx,
            &principal,
            project.path(),
            b"true",
            &environment,
            start,
        );
        assert!(bundle_exists(&stale));

        // The next mint after expiry deletes the unused bundle.
        let later = start + ESCALATION_GRANT_TTL;
        let fresh = mint_test_grant(
            &ctx,
            &principal,
            project.path(),
            b"true",
            &environment,
            later,
        );
        assert!(!bundle_exists(&stale), "expired payload bundle was kept");
        assert!(bundle_exists(&fresh));
        let replay = grant_attempt(stale, project.path(), b"true", environment.clone());
        assert_eq!(
            consume_host_escalation_grant_at(&ctx, &principal, &replay, later)
                .err()
                .map(EscalationRefusal::class),
            Some("expired")
        );

        // A refused attempt uses the grant up, so its bundle goes too.
        let mut wrong_root = grant_attempt(fresh.clone(), project.path(), b"true", environment);
        wrong_root.root = elsewhere.path().to_path_buf();
        assert_eq!(
            consume_host_escalation_grant_at(&ctx, &principal, &wrong_root, later)
                .err()
                .map(EscalationRefusal::class),
            Some("digest_mismatch")
        );
        assert!(!bundle_exists(&fresh), "refused payload bundle was kept");
        assert_eq!(ctx.escalation_grants().lock().len_for_test(), 2);
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
                "sandbox profile strict is not enforced on Windows; select sandbox.profile off or run on macOS/Linux"
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
                "sandbox profile strict is not enforced on Windows; select sandbox.profile off or run on macOS/Linux"
            )
        );
    }
}

#[cfg(test)]
mod read_allow_tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn vanished_listed_sibling_is_dropped_without_widening_read_access() {
        struct RemovingLister {
            parent: PathBuf,
            sibling: PathBuf,
            removed: bool,
        }
        impl ReadDirectoryLister for RemovingLister {
            fn children(&mut self, parent: &Path) -> std::io::Result<Vec<ListedReadChild>> {
                let children = SecureReadDirectoryLister.children(parent)?;
                if parent == self.parent {
                    assert!(children.iter().any(|child| child.path == self.sibling));
                    std::fs::remove_dir_all(&self.sibling).expect("remove listed sibling");
                    self.removed = true;
                }
                Ok(children)
            }
        }

        let root = tempfile::tempdir().expect("fixture");
        let root = root.path().canonicalize().expect("canonical fixture");
        let shared = root.join("shared");
        let vanished = shared.join("vanished");
        let kept = shared.join("kept");
        let denied = shared.join("denied");
        let project = root.join("project");
        let temp = root.join("task-temp");
        for path in [&vanished, &kept, &denied, &project, &temp] {
            std::fs::create_dir_all(path).expect("fixture directory");
        }
        let profile = SandboxProfile::build(
            vec![project.clone()],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            vec![denied.clone()],
            Vec::new(),
            Vec::new(),
            temp.clone(),
        )
        .expect("base profile");
        let mut lister = RemovingLister {
            parent: shared.clone(),
            sibling: vanished.clone(),
            removed: false,
        };
        let profile = compute_linux_profile(profile, &shared, &[], &[], &mut lister)
            .expect("split shared read root");
        assert!(lister.removed, "deletion hook must run after enumeration");
        assert!(profile.read_allow.contains(&vanished));
        let listed_grants = profile.read_allow.clone();
        let profile = profile
            .canonicalize_for_launch()
            .expect("a vanished listed sibling must not refuse sandbox setup");
        assert_eq!(profile.read_allow, vec![project, kept, temp]);
        assert!(profile
            .read_allow
            .iter()
            .all(|path| listed_grants.contains(path)));
        assert!(!profile
            .read_allow
            .iter()
            .any(|path| denied.starts_with(path)));
        assert!(!profile.read_allow.contains(&shared));

        std::fs::create_dir(&vanished).expect("recreate removed sibling");
        let new_sibling = shared.join("new-sibling");
        std::fs::create_dir(&new_sibling).expect("create later sibling");
        for path in [&vanished, &new_sibling, &denied] {
            assert!(!profile
                .read_allow
                .iter()
                .any(|grant| path.starts_with(grant)));
        }
        let transferred: SandboxProfile =
            serde_json::from_slice(&serde_json::to_vec(&profile).unwrap()).unwrap();
        assert_eq!(transferred.canonicalize_for_launch().unwrap(), profile);
    }

    #[derive(Default)]
    struct FakeLister {
        entries: BTreeMap<PathBuf, Result<Vec<ListedReadChild>, (std::io::ErrorKind, String)>>,
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
            self.entries.insert(
                PathBuf::from(parent),
                Err((std::io::ErrorKind::Other, message.to_string())),
            );
            self
        }

        fn missing(mut self, parent: &str) -> Self {
            self.entries.insert(
                PathBuf::from(parent),
                Err((std::io::ErrorKind::NotFound, "directory vanished".into())),
            );
            self
        }
    }

    impl ReadDirectoryLister for FakeLister {
        fn children(&mut self, parent: &Path) -> std::io::Result<Vec<ListedReadChild>> {
            self.entries
                .get(parent)
                .cloned()
                .unwrap_or_else(|| {
                    Err((
                        std::io::ErrorKind::Other,
                        format!("unexpected enumeration of {}", parent.display()),
                    ))
                })
                .map_err(|(kind, message)| std::io::Error::new(kind, message))
        }
    }

    fn grant(path: &str, force_children: bool, mandatory: bool) -> IntendedReadGrant {
        IntendedReadGrant {
            path: PathBuf::from(path),
            force_children,
            mandatory,
        }
    }

    /// Canonical directory inventory independent of the host filesystem. The
    /// connection is a regular file outside CortexKit data, just as in the
    /// production spawn fixture. Private subtrees are intentionally absent from
    /// the inventory: policy may grant exact exceptions, never enumerate them.
    fn linux_profile_model() -> (SandboxProfile, FakeLister, Vec<IntendedReadGrant>) {
        let data = "/home/alice/.local/share/cortexkit";
        let storage = format!("{data}/aft");
        let project = format!("{data}/alfonso/worktrees/own");
        let session = format!("{storage}/opencode/bash-tasks/current");
        let temp = format!("{session}/bash-own/io/aft-sandbox-own");
        let shims = format!("{storage}/shims");
        let hooks = format!("{storage}/cache/git-hooks/current");
        let binary = format!("{data}/bin/ck-aft");
        let connection = PathBuf::from("/home/alice/daemon/connection.json");
        let profile = SandboxProfile {
            v: crate::sandbox_profile::SANDBOX_PROFILE_VERSION,
            mode: crate::config::SandboxMode::Strict,
            worker_jail: false,
            keychain_deny: Vec::new(),
            deny_prefixes: Vec::new(),
            data_policy: crate::sandbox_profile::SandboxDataPolicy {
                deny: vec![
                    data.into(),
                    "/home/alice/.local/state/cortexkit".into(),
                    storage.into(),
                    session.into(),
                ],
                read_allow: vec![
                    project.clone().into(),
                    temp.clone().into(),
                    shims.clone().into(),
                    hooks.clone().into(),
                    binary.clone().into(),
                ],
                write_allow: vec![project.clone().into(), temp.clone().into()],
                read_metadata_allow: Vec::new(),
                session_task_read_dir: None,
            },
            writable_roots: vec![project.into()],
            writable_files: Vec::new(),
            write_deny: vec![
                connection.clone(),
                "/home/alice/.ssh".into(),
                shims.into(),
                hooks.into(),
                binary.into(),
            ],
            write_deny_nested: Vec::new(),
            read_allow: Vec::new(),
            read_allow_listed: Vec::new(),
            read_deny: vec![
                connection,
                "/home/alice/.ssh".into(),
                "/run/user".into(),
                "/run/credentials".into(),
                "/run/secrets".into(),
            ],
            socket_deny: Vec::new(),
            loopback_tcp_connect_deny: Vec::new(),
            cache_roots: vec!["/home/alice/.npm".into()],
            temp_dir: temp.into(),
            worker_gh_fence: None,
        };
        let inventory = FakeLister::default()
            .directory(
                "/home/alice",
                &[
                    (".ssh", true),
                    (".local", true),
                    ("daemon", true),
                    ("notes", false),
                    (".npm", true),
                ],
            )
            .directory(
                "/home/alice/daemon",
                &[("connection.json", false), ("readme", false)],
            )
            .directory("/home/alice/.local", &[("share", true), ("state", true)])
            .directory(
                "/home/alice/.local/share",
                &[("cortexkit", true), ("other-program", true)],
            )
            .directory(
                "/home/alice/.local/state",
                &[("cortexkit", true), ("other-state", true)],
            )
            .directory(
                "/run",
                &[
                    ("user", true),
                    ("credentials", true),
                    ("secrets", true),
                    ("lock", true),
                ],
            );
        (
            profile,
            inventory,
            vec![grant("/usr", false, true), grant("/run", false, true)],
        )
    }

    #[test]
    fn linux_profile_private_tree_carve_outs_are_exact() {
        let (profile, mut inventory, base) = linux_profile_model();
        let profile = compute_linux_profile(
            profile,
            Path::new("/home/alice"),
            &[],
            &base,
            &mut inventory,
        )
        .expect("a worktree inside private data must produce a usable Linux profile");
        let mut expected = [
            "/usr", "/run/lock", "/home/alice/.npm", "/home/alice/notes", "/home/alice/daemon/readme",
            "/home/alice/.local/share/other-program", "/home/alice/.local/state/other-state",
            "/home/alice/.local/share/cortexkit/alfonso/worktrees/own",
            "/home/alice/.local/share/cortexkit/aft/opencode/bash-tasks/current/bash-own/io/aft-sandbox-own",
            "/home/alice/.local/share/cortexkit/aft/shims",
            "/home/alice/.local/share/cortexkit/aft/cache/git-hooks/current",
            "/home/alice/.local/share/cortexkit/bin/ck-aft",
        ].map(PathBuf::from).to_vec();
        expected.sort_unstable();
        assert_eq!(
            profile.read_allow, expected,
            "read/execute rules must be exact"
        );
        let writes = profile.write_allow_roots();
        assert_eq!(writes, vec![
            Path::new("/home/alice/.local/share/cortexkit/aft/opencode/bash-tasks/current/bash-own/io/aft-sandbox-own"),
            Path::new("/home/alice/.local/share/cortexkit/alfonso/worktrees/own"),
            Path::new("/home/alice/.npm"),
        ]);
        for deny in &profile.data_policy.deny {
            assert!(
                !profile
                    .read_allow
                    .iter()
                    .any(|grant| deny.starts_with(grant)),
                "a read grant covers private tree {}",
                deny.display()
            );
            assert!(
                !writes.iter().any(|grant| deny.starts_with(grant)),
                "a write grant covers private tree {}",
                deny.display()
            );
        }
        // These victim paths are deliberately outside every exact exception.
        for victim in [
            "/home/alice/daemon/connection.json",
            "/home/alice/.local/share/cortexkit/other-module/store",
            "/home/alice/.local/share/cortexkit/aft/undo/history",
            "/home/alice/.local/share/cortexkit/alfonso/worktrees/other/file",
            "/home/alice/.local/share/cortexkit/aft/opencode/bash-tasks/other/io/stdout",
            "/home/alice/.local/state/cortexkit/gh-shim/manifest.json",
        ] {
            assert!(
                !profile
                    .read_allow
                    .iter()
                    .any(|grant| Path::new(victim).starts_with(grant)),
                "readable victim: {victim}"
            );
            assert!(
                !writes
                    .iter()
                    .any(|grant| Path::new(victim).starts_with(grant)),
                "writable victim: {victim}"
            );
        }
        for managed in [
            "/home/alice/.local/share/cortexkit/aft/shims",
            "/home/alice/.local/share/cortexkit/aft/cache/git-hooks/current",
            "/home/alice/.local/share/cortexkit/bin/ck-aft",
        ] {
            assert!(
                profile.read_allow.contains(&PathBuf::from(managed)),
                "missing managed read/execute grant: {managed}"
            );
            assert!(
                !writes
                    .iter()
                    .any(|grant| paths_overlap(grant, Path::new(managed))),
                "managed executable writable: {managed}"
            );
        }
    }

    #[test]
    fn linux_profile_repository_hooks_preserve_existing_child_reads() {
        let (mut profile, mut inventory, base) = linux_profile_model();
        let project = "/home/alice/.local/share/cortexkit/alfonso/worktrees/own";
        let git = format!("{project}/.git");
        profile.read_deny.push(format!("{git}/hooks").into());
        inventory = inventory
            .directory(
                project,
                &[("tracked", false), ("src", true), (".git", true)],
            )
            .directory(
                &git,
                &[
                    ("hooks", true),
                    ("objects", true),
                    ("refs", true),
                    ("index", false),
                ],
            );
        let profile = compute_linux_profile(
            profile,
            Path::new("/home/alice"),
            &[git.into()],
            &base,
            &mut inventory,
        )
        .expect("nested hooks must not make the entire project unavailable");
        assert!(profile.write_allow_roots().contains(&Path::new(project)));
        for readable in ["tracked", "src", ".git/objects", ".git/refs", ".git/index"] {
            assert!(
                profile
                    .read_allow
                    .contains(&Path::new(project).join(readable)),
                "missing project child: {readable}"
            );
        }
        for denied in [".git/hooks/pre-commit", "new-at-project-root"] {
            let denied = Path::new(project).join(denied);
            assert!(
                !profile
                    .read_allow
                    .iter()
                    .any(|grant| denied.starts_with(grant)),
                "unexpected readable path: {}",
                denied.display()
            );
        }
    }

    #[test]
    fn linux_profile_broad_base_read_grant_is_split_around_private_data() {
        let (profile, mut inventory, mut base) = linux_profile_model();
        // Even another source of read access to the parent must be split, not
        // emitted wholesale alongside the precise private-tree exceptions.
        base.push(grant("/home/alice/.local/share", false, true));
        let profile = compute_linux_profile(
            profile,
            Path::new("/home/alice"),
            &[],
            &base,
            &mut inventory,
        )
        .expect("a broad base read grant can be split without refusing the worktree");
        assert!(profile.read_allow.contains(&PathBuf::from(
            "/home/alice/.local/share/cortexkit/alfonso/worktrees/own"
        )));
        assert!(profile
            .read_allow
            .contains(&PathBuf::from("/home/alice/.local/share/other-program")));
        assert!(!profile
            .read_allow
            .iter()
            .any(|grant| Path::new("/home/alice/.local/share/cortexkit").starts_with(grant)));
    }

    #[test]
    fn linux_profile_connection_file_inside_project_still_refuses() {
        let (mut profile, mut inventory, base) = linux_profile_model();
        let connection = profile.writable_roots[0].join("connection.json");
        profile.read_deny.push(connection.clone());
        profile.write_deny.push(connection);
        let error = compute_linux_profile(
            profile,
            Path::new("/home/alice"),
            &[],
            &base,
            &mut inventory,
        )
        .expect_err("private-tree exceptions must not override credential denies");
        assert!(error.contains("overlaps mandatory secret floor"), "{error}");
    }

    #[test]
    fn vanished_listed_deny_ancestor_is_omitted_during_split() {
        let mut lister = FakeLister::default()
            .directory("/shared", &[("vanished", true), ("kept", true)])
            .missing("/shared/vanished");
        let denies = vec![PathBuf::from("/shared/vanished/secret")];
        let (paths, listed) = split_read_grants_with_inventory(
            &[grant("/shared", true, false)],
            &denies,
            &mut lister,
        )
        .expect("a listed child disappearing before recursion only removes its grant");
        assert_eq!(paths, vec![PathBuf::from("/shared/kept")]);
        assert_eq!(listed, paths);
        assert!(
            split_read_grants(
                &[grant("/shared/vanished", true, false)],
                &denies,
                &mut lister,
            )
            .is_err(),
            "an explicitly requested missing directory still refuses"
        );
    }

    #[test]
    fn direct_read_grant_remains_required_when_also_listed() {
        let mut lister = FakeLister::default().directory("/shared", &[("required", false)]);
        for grants in [
            vec![
                grant("/shared", true, false),
                grant("/shared/required", false, false),
            ],
            vec![
                grant("/shared/required", false, false),
                grant("/shared", true, false),
            ],
        ] {
            let (paths, listed) =
                split_read_grants_with_inventory(&grants, &[], &mut lister).unwrap();
            assert_eq!(paths, vec![PathBuf::from("/shared/required")]);
            assert!(
                listed.is_empty(),
                "direct provenance wins regardless of grant order"
            );
        }
    }

    #[test]
    fn denied_connection_file_is_omitted_without_refusing_readable_siblings() {
        let mut lister = FakeLister::default().directory(
            "/home/alice/daemon",
            &[("connection.json", false), ("ordinary.txt", false)],
        );
        let emitted = split_read_grants(
            &[grant("/home/alice/daemon", false, false)],
            &[PathBuf::from("/home/alice/daemon/connection.json")],
            &mut lister,
        )
        .expect("an exact denied file must be omitted, not treated as a directory");
        assert_eq!(
            emitted,
            vec![PathBuf::from("/home/alice/daemon/ordinary.txt")]
        );
    }

    #[test]
    fn deny_chain_crossing_regular_file_still_refuses() {
        let mut lister =
            FakeLister::default().directory("/home/alice", &[("not-a-directory", false)]);
        let error = split_read_grants(
            &[grant("/home/alice", true, false)],
            &[PathBuf::from("/home/alice/not-a-directory/secret")],
            &mut lister,
        )
        .expect_err("a strict descendant deny cannot be represented through a regular file");
        assert!(
            error.contains("deny chain crosses non-directory path"),
            "{error}"
        );
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

#[cfg(all(test, unix))]
mod worker_gh_fence_tests {
    use super::*;
    use crate::sandbox_profile::WorkerGhFence;

    fn fenced_profile(canary_dir: &Path) -> SandboxProfile {
        SandboxProfile {
            v: crate::sandbox_profile::SANDBOX_PROFILE_VERSION,
            mode: crate::config::SandboxMode::Strict,
            worker_jail: true,
            keychain_deny: Vec::new(),
            deny_prefixes: Vec::new(),
            data_policy: Default::default(),
            writable_roots: vec![PathBuf::from("/home/alice/work")],
            writable_files: Vec::new(),
            write_deny: vec![canary_dir.to_path_buf()],
            write_deny_nested: Vec::new(),
            read_allow: vec![PathBuf::from("/usr"), PathBuf::from("/home/alice/work")],
            read_allow_listed: Vec::new(),
            read_deny: vec![canary_dir.to_path_buf()],
            socket_deny: Vec::new(),
            loopback_tcp_connect_deny: Vec::new(),
            cache_roots: Vec::new(),
            temp_dir: PathBuf::from("/home/alice/work/tmp"),
            worker_gh_fence: Some(WorkerGhFence::Canary {
                paths: vec![canary_dir.join(crate::worker_jail_canary::CANARY_FILE)],
            }),
        }
    }

    const CANARY: &str = "/home/alice/.local/share/cortexkit/aft/gh-shim-worker-jail";

    #[test]
    fn a_fenced_worker_profile_passes_only_with_the_canary_denied_and_ungranted() {
        let dir = Path::new(CANARY);
        assert!(verify_worker_gh_fence(&fenced_profile(dir)).is_ok());

        let mut no_read_deny = fenced_profile(dir);
        no_read_deny.read_deny.clear();
        assert!(verify_worker_gh_fence(&no_read_deny).is_err());

        let mut no_write_deny = fenced_profile(dir);
        no_write_deny.write_deny.clear();
        assert!(verify_worker_gh_fence(&no_write_deny).is_err());

        // Landlock grants are whole subtrees: a grant over any ancestor makes
        // the canary readable, and a writable ancestor lets a worker rename it.
        for ancestor in ["/home/alice", "/home/alice/.local/share", CANARY] {
            let mut read = fenced_profile(dir);
            read.read_allow.push(PathBuf::from(ancestor));
            assert!(verify_worker_gh_fence(&read).is_err(), "read {ancestor}");
            let mut data = fenced_profile(dir);
            data.data_policy.read_allow.push(PathBuf::from(ancestor));
            assert!(verify_worker_gh_fence(&data).is_err(), "data {ancestor}");
            let mut write = fenced_profile(dir);
            write.writable_roots.push(PathBuf::from(ancestor));
            assert!(verify_worker_gh_fence(&write).is_err(), "write {ancestor}");
        }
        // A sibling grant beside the canary is fine.
        let mut sibling = fenced_profile(dir);
        sibling.read_allow.push(PathBuf::from(
            "/home/alice/.local/share/cortexkit/aft/shims",
        ));
        assert!(verify_worker_gh_fence(&sibling).is_ok());
    }

    #[test]
    fn profiles_without_a_canary_fence_are_not_checked() {
        let mut head = fenced_profile(Path::new(CANARY));
        head.worker_gh_fence = None;
        head.read_allow.push(PathBuf::from("/home/alice"));
        assert!(verify_worker_gh_fence(&head).is_ok());
        head.worker_gh_fence = Some(WorkerGhFence::Unenforced);
        assert!(verify_worker_gh_fence(&head).is_ok());
    }

    #[test]
    fn every_home_anchor_must_be_denied() {
        let mut profile = fenced_profile(Path::new(CANARY));
        let other = PathBuf::from("/home/daemon/.local/share/cortexkit/aft/gh-shim-worker-jail");
        let Some(WorkerGhFence::Canary { paths }) = &mut profile.worker_gh_fence else {
            unreachable!()
        };
        paths.push(other.join(crate::worker_jail_canary::CANARY_FILE));
        assert!(verify_worker_gh_fence(&profile).is_err());
        profile.read_deny.push(other.clone());
        assert!(verify_worker_gh_fence(&profile).is_err());
        profile.write_deny.push(other);
        assert!(verify_worker_gh_fence(&profile).is_ok());
    }

    #[test]
    fn file_directory_and_ancestor_grants_refuse_worker_gh_fence() {
        let dir = Path::new(CANARY);
        for grant in [
            dir.join("canary"),
            dir.to_path_buf(),
            dir.parent().unwrap().into(),
        ] {
            for kind in 0..6 {
                let mut profile = fenced_profile(dir);
                match kind {
                    0 => profile.read_allow.push(grant.clone()),
                    1 => profile.data_policy.read_allow.push(grant.clone()),
                    2 => profile.writable_roots.push(grant.clone()),
                    3 => profile.writable_files.push(grant.clone()),
                    4 => profile.data_policy.write_allow.push(grant.clone()),
                    _ => profile.data_policy.session_task_read_dir = Some(grant.clone()),
                }
                assert!(
                    verify_worker_gh_fence(&profile).is_err(),
                    "grant {kind}: {}",
                    grant.display()
                );
            }
        }
    }
}

#[cfg(all(test, unix))]
mod credential_floor_tests {
    use super::*;

    #[test]
    fn credential_floor_protects_every_home_and_all_gh_config_locations() {
        let root = tempfile::tempdir().unwrap();
        let base = root.path().canonicalize().unwrap();
        let home = base.join("daemon");
        let account = base.join("account");
        let gh = base.join("custom-gh");
        let xdg = base.join("xdg");
        let (denies, cargo) = credential_floor_paths(&home, Some(&account), &[], |key| match key {
            "GH_CONFIG_DIR" => Some(gh.clone().into_os_string()),
            "XDG_CONFIG_HOME" => Some(xdg.clone().into_os_string()),
            _ => None,
        })
        .unwrap();
        for home in [&home, &account] {
            for relative in [
                ".ssh",
                ".aws",
                ".gnupg",
                ".azure",
                ".config/gh",
                ".config/gcloud",
                ".netrc",
                ".git-credentials",
                ".config/git/credentials",
                ".docker/config.json",
                ".npmrc",
                ".pypirc",
                ".kube/config",
                "Library/Keychains",
                ".config/cortexkit",
            ] {
                assert!(
                    denies.contains(&home.join(relative)),
                    "missing {}",
                    home.join(relative).display()
                );
            }
            assert!(cargo.contains(&home.join(".cargo")));
        }
        for path in [
            gh,
            xdg.join("gh"),
            xdg.join("gcloud"),
            xdg.join("git/credentials"),
        ] {
            assert!(denies.contains(&path), "missing {}", path.display());
        }
    }

    #[test]
    fn credential_floor_keeps_override_aliases_and_refuses_unresolvable_targets() {
        let root = tempfile::tempdir().unwrap();
        let base = root.path().canonicalize().unwrap();
        let real = base.join("real-gh");
        let alias = base.join("alias-gh");
        std::fs::create_dir(&real).unwrap();
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let (denies, _) = credential_floor_paths(&base, None, &[], |key| {
            (key == "GH_CONFIG_DIR").then(|| alias.clone().into_os_string())
        })
        .unwrap();
        assert!(denies.contains(&real));
        assert!(denies.contains(&alias));
        let loop_path = base.join("loop");
        std::os::unix::fs::symlink(&loop_path, &loop_path).unwrap();
        assert!(credential_floor_paths(&base, None, &[], |key| {
            (key == "GH_CONFIG_DIR").then(|| loop_path.clone().into_os_string())
        })
        .is_err());
    }
}

#[cfg(all(test, unix))]
mod worker_broker_floor_tests {
    use super::*;

    #[test]
    fn worker_gh_read_floor_xdg_config_is_denied_when_set() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let xdg = dir.path().join("xdg-config");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&xdg).unwrap();
        let (denies, _) = credential_floor_paths(&home, None, &[], |key| {
            (key == "XDG_CONFIG_HOME").then(|| xdg.as_os_str().to_owned())
        })
        .unwrap();
        assert!(denies.contains(&home.join(".config/cortexkit")));
        assert!(
            denies.contains(&xdg.join("cortexkit")),
            "the configured user config must remain credential-floored"
        );
    }

    #[test]
    fn worker_gh_read_floor_home_config_is_denied_when_xdg_unset() {
        let dir = tempfile::tempdir().unwrap();
        let (denies, _) = credential_floor_paths(dir.path(), None, &[], |_| None).unwrap();
        assert!(denies.contains(&dir.path().join(".config/cortexkit")));
    }

    #[test]
    fn worker_gh_read_floor_private_temps_deny_sibling_reads_without_changing_heads() {
        let dir = tempfile::tempdir().unwrap();
        let mut profile = SandboxProfile::build(
            vec![dir.path().to_owned()],
            vec![],
            vec![],
            vec![],
            vec![],
            vec![],
            vec![],
            dir.path().to_owned(),
        )
        .unwrap();
        let root = if cfg!(target_os = "macos") {
            PathBuf::from("/private/tmp/aft-w")
        } else {
            PathBuf::from("/tmp/aft-w")
        };
        profile.temp_dir = root.join("000000000001");
        profile
            .data_policy
            .read_allow
            .push(profile.temp_dir.clone());
        let foreign = root.join("000000000002/.gh-read-capability");
        profile
            .data_policy
            .read_allow
            .push(foreign.parent().unwrap().to_owned());
        profile.data_policy.write_allow.push(root.clone());
        let before = profile.data_policy.clone();
        with_worker_jail(false, || {
            protect_worker_credential_services(&mut profile, dir.path())
        })
        .unwrap();
        assert_eq!(
            profile.data_policy, before,
            "head shell access is unchanged"
        );
        with_worker_jail(true, || {
            protect_worker_credential_services(&mut profile, dir.path())
        })
        .unwrap();
        assert!(
            profile
                .data_policy
                .validate_grants(
                    [foreign.as_path()],
                    &profile.data_policy.read_allow,
                    &profile.writable_files
                )
                .is_err(),
            "a sibling capability must not be readable"
        );
        let own = profile.temp_dir.join(".gh-read-capability");
        assert!(
            profile
                .data_policy
                .validate_grants(
                    [own.as_path()],
                    &profile.data_policy.read_allow,
                    &profile.writable_files
                )
                .is_ok(),
            "the command must retain its own capability"
        );
        assert!(profile
            .data_policy
            .read_allow
            .iter()
            .filter(|path| path.starts_with(&root))
            .all(|path| path.starts_with(&profile.temp_dir)));
    }
}
