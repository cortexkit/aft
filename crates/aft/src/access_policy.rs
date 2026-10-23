//! Route-local filesystem authority. Resolution belongs to the caller; every
//! resolved target is decided here before a tool mutates it or a shell launches.

use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::sandbox_profile::{SandboxDataPolicy, SandboxProfile, SandboxProfileError};

thread_local! {
    static REQUEST_SCOPE: std::cell::RefCell<Option<crate::db::access_grants::ScopeKey>> = const { std::cell::RefCell::new(None) };
}

pub(crate) fn with_scope<R>(
    scope: Option<crate::db::access_grants::ScopeKey>,
    run: impl FnOnce() -> R,
) -> R {
    let _restore = scope_guard(scope);
    run()
}

pub(crate) struct ScopeGuard(
    Option<crate::db::access_grants::ScopeKey>,
    std::marker::PhantomData<*const ()>,
);

impl Drop for ScopeGuard {
    fn drop(&mut self) {
        REQUEST_SCOPE.with(|slot| {
            slot.replace(self.0.take());
        });
    }
}

pub(crate) fn scope_guard(scope: Option<crate::db::access_grants::ScopeKey>) -> ScopeGuard {
    ScopeGuard(
        REQUEST_SCOPE.with(|slot| slot.replace(scope)),
        std::marker::PhantomData,
    )
}

pub(crate) fn current_scope() -> Option<crate::db::access_grants::ScopeKey> {
    REQUEST_SCOPE.with(|slot| slot.borrow().clone())
}

/// Published crate sources (registry and git dependencies) and toolchains:
/// immutable package sources a worker reads when changing code against them.
/// Each entry is the home's environment variable, its default directory under
/// the user's home, and the subtree inside that home that may be read. The rest
/// of each home (credentials, config, `git/db`, registry cache and index) is not
/// granted.
const WORKER_RUST_ROOTS: [(&str, &str, &str); 3] = [
    ("CARGO_HOME", ".cargo", "registry/src"),
    ("CARGO_HOME", ".cargo", "git/checkouts"),
    ("RUSTUP_HOME", ".rustup", "toolchains"),
];

/// The built-in read grants for Rust library sources that every live worker
/// scope gets without asking. The daemon instance finds these roots in the
/// daemon's own environment, never a caller's. A root is pinned to its real directory the
/// first time it resolves, so later symlink changes cannot widen a grant. A
/// root that does not exist yet (no crate downloaded on a fresh machine) is
/// retried on the next policy build instead of being skipped until AFT
/// restarts.
pub(crate) struct WorkerRustSources {
    /// Finds a home directory from its environment variable name and its
    /// default directory under the user's home.
    locate_home: Box<dyn Fn(&str, &str) -> Option<PathBuf> + Send + Sync>,
    resolved: std::sync::Mutex<[Option<AccessGrant>; 3]>,
}

impl WorkerRustSources {
    fn new(locate_home: impl Fn(&str, &str) -> Option<PathBuf> + Send + Sync + 'static) -> Self {
        Self {
            locate_home: Box::new(locate_home),
            resolved: std::sync::Mutex::new([None, None, None]),
        }
    }

    /// The roots found from this daemon's environment.
    fn daemon() -> &'static Self {
        static DAEMON: std::sync::LazyLock<WorkerRustSources> = std::sync::LazyLock::new(|| {
            WorkerRustSources::new(|variable, default| {
                crate::environment::non_empty_os_var(variable)
                    .map(PathBuf::from)
                    .or_else(|| {
                        std::env::var_os("HOME")
                            .or_else(|| std::env::var_os("USERPROFILE"))
                            .map(|home| PathBuf::from(home).join(default))
                    })
            })
        });
        &DAEMON
    }

    fn grants(&self) -> Vec<AccessGrant> {
        let mut resolved = self
            .resolved
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for (slot, (variable, default, subtree)) in resolved.iter_mut().zip(WORKER_RUST_ROOTS) {
            if slot.is_none() {
                *slot = (self.locate_home)(variable, default)
                    .and_then(|root| resolve_worker_rust_root(&root, subtree));
            }
        }
        resolved.iter().flatten().cloned().collect()
    }
}

fn resolve_worker_rust_root(root: &Path, subtree: &str) -> Option<AccessGrant> {
    // The root is retried while it is missing, and a worker shell can write in
    // the Cargo cache, so it could create the subtree as a symlink to another
    // project before the next policy build. Grant only a real directory reached
    // without following any link below the configured home.
    let joined = root.join(subtree);
    if !no_symlink_below(root, &joined) {
        return None;
    }
    let path = joined.canonicalize().ok()?;
    path.is_dir().then(|| AccessGrant {
        path: crate::inspect::job::normalize_path(&path),
        kind: GrantKind::Subtree,
        operation: AccessOperation::Read,
    })
}

/// True when every component of `path` below `root` exists and is a real
/// directory, not a symlink or junction. `root` itself may be a link (a moved
/// CARGO_HOME is normal); only what sits inside it must be genuine.
fn no_symlink_below(root: &Path, path: &Path) -> bool {
    let Ok(rest) = path.strip_prefix(root) else {
        return false;
    };
    let mut current = root.to_path_buf();
    for component in rest.components() {
        current.push(component);
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_dir() => {}
            _ => return false,
        }
    }
    true
}

/// Who answers an `ask` decision for the call running on this thread.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum AskRoute {
    /// CLI/library callers without a host retain their configured legacy access.
    /// Subc and native OpenCode/Pi entry points install an explicit route.
    #[default]
    Lowered,
    /// A trusted plugin presents engine-generated asks and echoes approved IDs.
    Plugin {
        scope: plugin::Scope,
        once: Vec<AccessGrant>,
    },
    /// An untrusted consumer may approve capabilities only over reverse calls.
    Elicitation { once: Vec<AccessGrant> },
    /// No permission UI is available on this route.
    Deny,
    /// A stamped Alfonso worker: an `ask` refuses with `access_ask_required`
    /// so the subc layer can put the question to Cingulate. `once` holds the
    /// capabilities an `allow_once` answer approved for this one dispatch.
    Consent {
        once: Vec<AccessGrant>,
        /// Identifies the saved launch whose grants need restoration. Until
        /// restored, those grants may explain refusals but cannot authorize access.
        launch: Option<crate::db::remote_exec::PolicyKey>,
    },
    /// A codemode program's call: nobody can be asked on its head agent's
    /// behalf yet, so an `ask` refuses with [`PERMISSION_ASK_UNAVAILABLE`],
    /// naming the permission and the path. Allow and deny decisions are
    /// unaffected.
    Unavailable,
}

/// Refusal for a codemode call that would need a permission ask.
pub(crate) const PERMISSION_ASK_UNAVAILABLE: &str = "permission_ask_unavailable";

#[path = "access_policy_plugin.rs"]
pub(crate) mod plugin;

#[path = "access_policy_temp.rs"]
pub(crate) mod temp;

pub struct CapturedAskRoute(AskRoute);
pub struct InstalledAskRoute {
    _guard: AskRouteGuard,
}

pub fn capture_ask_route() -> CapturedAskRoute {
    CapturedAskRoute(current_ask_route())
}

impl CapturedAskRoute {
    pub fn install(self) -> InstalledAskRoute {
        InstalledAskRoute {
            _guard: ask_route_guard(Some(self.0)),
        }
    }
}

/// Legacy stdin transports have no daemon bind identity. Their private process
/// context and configured harness supply a route; nested subc dispatch keeps
/// the authenticated route already installed by the daemon boundary.
pub fn with_native_route(
    mut request: crate::protocol::RawRequest,
    ctx: &crate::context::AppContext,
    run: impl FnOnce(
        crate::protocol::RawRequest,
        &crate::context::AppContext,
    ) -> crate::protocol::Response,
) -> crate::protocol::Response {
    let preview = request
        .params
        .get("preview")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
        || ["hashline_preflight", "undo_preview", "checkpoint_paths"]
            .contains(&request.command.as_str());
    match current_ask_route() {
        AskRoute::Plugin { scope, once } => {
            ctx.mark_standalone_plugin_host();
            strip_native_approval_metadata(&mut request);
            return finish_plugin_response(&scope, &once, preview, run(request, ctx));
        }
        AskRoute::Lowered => {}
        _ => return run(request, ctx),
    }
    let plugin = matches!(
        ctx.config().harness,
        Some(crate::harness::Harness::Opencode | crate::harness::Harness::Pi)
    );
    if !plugin {
        return run(request, ctx);
    }
    ctx.mark_standalone_plugin_host();
    let args = request
        .params
        .get("arguments")
        .or_else(|| request.params.get("args"))
        .unwrap_or(&request.params);
    let tool = native_tool_name(&request);
    let mut scope = ctx
        .standalone_approval_scope(request.session())
        .for_tool(tool, args);
    let ids: Vec<String> = args
        .get("permissions_granted")
        .and_then(serde_json::Value::as_array)
        .map(|ids| {
            ids.iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let once = plugin::claim(&mut scope, &ids);
    strip_native_approval_metadata(&mut request);
    let _route = ask_route_guard(Some(AskRoute::Plugin {
        scope: scope.clone(),
        once: once.clone(),
    }));
    finish_plugin_response(&scope, &once, preview, run(request, ctx))
}

/// The tool a native request runs: the nested `name` of a `tool_call`,
/// otherwise the command itself.
fn native_tool_name(request: &crate::protocol::RawRequest) -> &str {
    if request.command == "tool_call" {
        request
            .params
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
    } else {
        &request.command
    }
}

/// Remove `permissions_granted` before a plugin host's native request runs,
/// after its outside-access tokens were claimed.
///
/// On the native wire `permissions_granted` only ever holds the host's own
/// approvals: both plugins drop the agent's `permissions_granted` from the
/// tool arguments before adding the approvals they granted
/// (`withHostPermissionGrants` and `withOutsidePermissions` in
/// `packages/opencode-plugin/src/tools/outside-permissions.ts` and
/// `packages/pi-plugin/src/tools/_shared.ts`, each destructuring the field
/// away as `_untrusted`). A shell reads its approved command patterns and
/// escalation grants from that field, so it is kept when the tool that
/// runs is bash or powershell, called directly or nested in a `tool_call`.
/// Every other tool has no use for it, so it is removed and a forged value
/// can never reach a handler.
fn strip_native_approval_metadata(request: &mut crate::protocol::RawRequest) {
    if matches!(native_tool_name(request), "bash" | "powershell") {
        return;
    }
    let params = &mut request.params;
    let arguments = if params.get("arguments").is_some() {
        params.get_mut("arguments")
    } else if params.get("args").is_some() {
        params.get_mut("args")
    } else {
        Some(params)
    };
    if let Some(args) = arguments.and_then(serde_json::Value::as_object_mut) {
        args.remove("permissions_granted");
    }
}

pub(crate) fn finish_plugin_response(
    scope: &plugin::Scope,
    once: &[AccessGrant],
    preview: bool,
    mut response: crate::protocol::Response,
) -> crate::protocol::Response {
    if !once.is_empty()
        && (preview || response.data["code"] == "permission_required")
        && response.data.get("permissions_continuation").is_none()
    {
        response.data["permissions_continuation"] =
            serde_json::json!(plugin::continuation(scope, once));
    }
    response
}

pub(crate) fn finish_current_plugin_response(
    response: crate::protocol::Response,
) -> crate::protocol::Response {
    match current_ask_route() {
        AskRoute::Plugin { scope, once } => finish_plugin_response(&scope, &once, false, response),
        _ => response,
    }
}

/// Internal refusal code for an `ask` that must go to Cingulate. The subc
/// layer turns it into a held call; it never reaches the agent as-is.
pub(crate) const ACCESS_ASK_REQUIRED: &str = "access_ask_required";

/// Whether `response` asks for access instead of failing: a plugin host's
/// outside-access ask (`permission_required` with the `asks` the host prompts
/// with, then retries) or a worker call held for consent through Cingulate
/// (`access_ask_required`). A handler that folds errors into a phase failure
/// or a batch entry must return such a response unchanged, or nobody can be
/// asked and the call can never run.
pub(crate) fn is_access_request(response: &crate::protocol::Response) -> bool {
    matches!(
        response
            .data
            .get("code")
            .and_then(serde_json::Value::as_str),
        Some(crate::protocol::ERROR_PERMISSION_REQUIRED | ACCESS_ASK_REQUIRED)
    )
}

thread_local! {
    static ASK_ROUTE: std::cell::RefCell<Option<AskRoute>> = const { std::cell::RefCell::new(None) };
}

pub(crate) struct AskRouteGuard(Option<AskRoute>, std::marker::PhantomData<*const ()>);

impl Drop for AskRouteGuard {
    fn drop(&mut self) {
        ASK_ROUTE.with(|slot| {
            slot.replace(self.0.take());
        });
    }
}

pub(crate) fn ask_route_guard(route: Option<AskRoute>) -> AskRouteGuard {
    AskRouteGuard(
        ASK_ROUTE.with(|slot| slot.replace(route)),
        std::marker::PhantomData,
    )
}

pub(crate) fn current_ask_route() -> AskRoute {
    ASK_ROUTE
        .with(|slot| slot.borrow().clone())
        .unwrap_or_default()
}

impl AccessGrant {
    pub(crate) fn covers_kind(
        &self,
        operation: AccessOperation,
        path: &Path,
        kind: GrantKind,
    ) -> bool {
        self.covers(operation, path) && (self.kind == GrantKind::Subtree || kind == GrantKind::File)
    }
    /// Whether this capability lets `operation` touch `path`. Write implies
    /// read, and a subtree matches by whole path component.
    pub fn covers(&self, operation: AccessOperation, path: &Path) -> bool {
        (self.operation == AccessOperation::Write || operation == AccessOperation::Read)
            && match self.kind {
                GrantKind::File => path == self.path,
                GrantKind::Subtree => path.starts_with(&self.path),
            }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccessOperation {
    Read,
    Write,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessDecision {
    Allow,
    Ask,
    Deny(AccessDenial),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessDenial {
    Floor,
    OutsideRoots,
    ReadOnly,
    UnsafeTemp,
}

/// What a route does with a path outside every one of its roots.
///
/// The user-facing spelling is the `restrict_to_project_root` setting:
/// [`OutsideRoots::from_config`] is the one place the engine reads it, and
/// every check that used to test the setting asks this rule instead.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OutsideRoots {
    /// An outside path takes the route's ask path. This is the default.
    #[default]
    Ask,
    /// An outside path that no floor, grant or other explicit allowance
    /// already decided is refused with `path_outside_root` instead of asked.
    /// Checks that follow symlinks resolve them before comparing with the
    /// root, so a link cannot lead out of it; write-location checks keep the
    /// final component and judge the link itself. The standalone exemptions
    /// (a linked worktree's main checkout, system temp scratch) do not apply.
    Deny,
}

impl OutsideRoots {
    /// The configured rule: `restrict_to_project_root: true` is
    /// "outside roots: deny"; absent or `false` is "outside roots: ask".
    pub fn from_config(config: &Config) -> Self {
        if config.restrict_to_project_root {
            Self::Deny
        } else {
            Self::Ask
        }
    }

    /// The rule for one request: an untrusted route may force deny for its
    /// own requests even when the configured rule asks.
    pub fn for_request(config: &Config, forced: bool) -> Self {
        if forced {
            Self::Deny
        } else {
            Self::from_config(config)
        }
    }

    /// The policy decision for an outside path. Without a project root
    /// there is no root to be outside of, so a configured deny has nothing
    /// to refuse and outside paths keep the ask path.
    fn decision(self, has_project_root: bool) -> AccessDecision {
        match self {
            Self::Deny if has_project_root => AccessDecision::Deny(AccessDenial::OutsideRoots),
            _ => AccessDecision::Ask,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalMode {
    FullAccess,
    #[default]
    Ask,
    ReadOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrantKind {
    File,
    Subtree,
}

/// Canonical path capabilities supplied by the route owner. Persistence and
/// consent cannot override denied credential paths, read_deny, or private stores.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AccessGrant {
    pub path: PathBuf,
    pub kind: GrantKind,
    #[serde(rename = "mode")]
    pub operation: AccessOperation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HeadPolicy {
    Allow,
    NotHeld,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum FloorRule {
    CredentialPath,
    CortexkitConfig,
    CortexkitModuleData,
    CortexkitModuleState,
    AftStorage,
    #[cfg(unix)]
    SessionStore,
    SubcConnection,
    ConfiguredReadDeny,
    TaskEnvironment,
    PolicyFloor,
}

impl FloorRule {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::CredentialPath => "credential_path",
            Self::CortexkitConfig => "cortexkit_config",
            Self::CortexkitModuleData => "cortexkit_module_data",
            Self::CortexkitModuleState => "cortexkit_module_state",
            Self::AftStorage => "aft_storage",
            #[cfg(unix)]
            Self::SessionStore => "session_store",
            Self::SubcConnection => "subc_connection",
            Self::ConfiguredReadDeny => "configured_read_deny",
            Self::TaskEnvironment => "task_environment",
            Self::PolicyFloor => "policy_floor",
        }
    }

    pub(crate) fn description(self) -> &'static str {
        match self {
            Self::CredentialPath => "credential path",
            Self::CortexkitConfig => "CortexKit configuration store",
            Self::CortexkitModuleData => {
                "module data store under ~/.local/share/cortexkit/<module>"
            }
            Self::CortexkitModuleState => {
                "module state store under ~/.local/state/cortexkit/<module>"
            }
            Self::AftStorage => "AFT storage directory",
            #[cfg(unix)]
            Self::SessionStore => "session storage directory",
            Self::SubcConnection => "subc connection credentials",
            Self::ConfiguredReadDeny => "configured sandbox read deny",
            Self::TaskEnvironment => "private bash task environment",
            Self::PolicyFloor => "protected filesystem path",
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct PolicyFloor {
    pub(crate) path: PathBuf,
    pub(crate) rule: FloorRule,
}

#[derive(Debug, Clone)]
pub struct AccessPolicy {
    pub roots: Vec<PathBuf>,
    pub mode: ApprovalMode,
    pub outside: AccessDecision,
    pub absolute_floor: Vec<PathBuf>,
    floor_rules: Vec<PolicyFloor>,
    /// Built-in protected paths only. User read_deny retains the native shell's
    /// existing read-only restriction rather than denying the enclosing write root.
    pub shell_write_floor: Vec<PathBuf>,
    pub private_stores: SandboxDataPolicy,
    pub grants: Vec<AccessGrant>,
    /// Shell-only configured write exceptions; they do not grant access to private stores.
    pub shell_write_allow: Vec<PathBuf>,
    /// Checkouts that live inside a private store but hold project code, not
    /// private data: Alfonso's worker worktrees. They leave the private-store
    /// floor and fall back to the ordinary outside-root rules, so a head can
    /// review a worker's files. The rest of the store stays denied.
    pub private_store_carveouts: Vec<PathBuf>,
    /// Standalone scratch allowances; never override floors or hard restriction.
    pub standalone_temp_roots: Vec<PathBuf>,
    pub standalone_temp_guard_roots: Vec<PathBuf>,
    pub standalone_managed_temp_roots: Vec<PathBuf>,
    pub standalone_owned_scratch: bool,
    /// Native host compatibility roots, not additional shell write roots.
    pub standalone_main_roots: Vec<PathBuf>,
}

impl AccessPolicy {
    /// Snapshot the route configuration. Unrestricted outside access is represented
    /// as Ask and lowered to the existing behavior by the in-process caller.
    pub fn from_config(config: &Config, storage: &Path, roots: Vec<PathBuf>, home: &Path) -> Self {
        let mut floor_rules = Vec::new();
        let roots = roots
            .into_iter()
            .map(|root| crate::inspect::job::normalize_path(&root))
            .collect::<Vec<_>>();
        let shell_write_floor = [
            (".ssh", FloorRule::CredentialPath),
            (".aws", FloorRule::CredentialPath),
            (".gnupg", FloorRule::CredentialPath),
            (".config/gcloud", FloorRule::CredentialPath),
            (".azure", FloorRule::CredentialPath),
            (".config/cortexkit", FloorRule::CortexkitConfig),
        ]
        .into_iter()
        .map(|(path, rule)| {
            let path = policy_path(&home.join(path));
            floor_rules.push(PolicyFloor {
                path: path.clone(),
                rule,
            });
            path
        })
        .collect::<Vec<_>>();
        #[cfg(target_os = "linux")]
        let shell_write_floor = shell_write_floor
            .into_iter()
            .chain(
                [
                    PathBuf::from("/run/user"),
                    PathBuf::from("/run/credentials"),
                    PathBuf::from("/run/secrets"),
                ]
                .into_iter()
                .map(|path| {
                    let path = policy_path(&path);
                    floor_rules.push(PolicyFloor {
                        path: path.clone(),
                        rule: FloorRule::CredentialPath,
                    });
                    path
                }),
            )
            .collect::<Vec<_>>();
        let mut absolute_floor = shell_write_floor.clone();
        absolute_floor.extend(config.sandbox.read_deny.iter().map(|path| {
            let path = policy_path(&expand_home(path, home));
            floor_rules.push(PolicyFloor {
                path: path.clone(),
                rule: FloorRule::ConfiguredReadDeny,
            });
            path
        }));
        let mut deny: Vec<_> = [
            (
                home.join(".local/share/cortexkit"),
                FloorRule::CortexkitModuleData,
            ),
            (
                home.join(".local/state/cortexkit"),
                FloorRule::CortexkitModuleState,
            ),
            (storage.to_path_buf(), FloorRule::AftStorage),
            (
                home.join(".local/share/cortexkit/run/subc-connection.json"),
                FloorRule::SubcConnection,
            ),
        ]
        .into_iter()
        .map(|(path, rule)| {
            let path = policy_path(&path);
            floor_rules.push(PolicyFloor {
                path: path.clone(),
                rule,
            });
            path
        })
        .collect();
        if let Some(path) = &config.semantic.subc_connection_file {
            let path = expand_home(path, home);
            let path = policy_path(&if path.is_absolute() {
                path
            } else {
                home.join(path)
            });
            floor_rules.push(PolicyFloor {
                path: path.clone(),
                rule: FloorRule::SubcConnection,
            });
            deny.push(path);
        }
        for floor in environment_private_store_floors(|key| std::env::var_os(key)) {
            deny.push(floor.path.clone());
            floor_rules.push(floor);
        }
        let mut private_store_carveouts =
            vec![home.join(".local/share/cortexkit/alfonso/worktrees")];
        if let Some(path) = crate::environment::non_empty_os_var("XDG_DATA_HOME") {
            let path = PathBuf::from(path);
            if path.is_absolute() {
                private_store_carveouts.push(path.join("cortexkit/alfonso/worktrees"));
            }
        }
        let mut read_allow = roots.clone();
        let mut managed_paths = Vec::new();
        if config.github.shim {
            managed_paths.push(storage.join(crate::agent_child_env::SHIMS_DIR_NAME));
            if let Ok(binary) = crate::agent_child_env::shim_binary(config) {
                managed_paths.push(binary);
            }
        }
        if config.git.co_author != "off" {
            managed_paths.push(crate::agent_child_env::managed_git_hooks_dir(storage));
            if config.git.co_author == "auto" {
                if let Ok(binary) = crate::agent_child_env::shim_binary(config) {
                    managed_paths.push(binary);
                }
            }
        }
        read_allow.extend(
            managed_paths
                .into_iter()
                .filter(|path| path.exists())
                .map(|path| policy_path(&path)),
        );
        Self {
            standalone_temp_roots: Vec::new(),
            standalone_temp_guard_roots: Vec::new(),
            standalone_managed_temp_roots: Vec::new(),
            standalone_owned_scratch: false,
            standalone_main_roots: Vec::new(),
            mode: ApprovalMode::Ask,
            private_stores: SandboxDataPolicy {
                deny,
                read_allow,
                write_allow: roots.clone(),
                read_metadata_allow: Vec::new(),
                session_task_read_dir: None,
            },
            roots,
            outside: OutsideRoots::from_config(config).decision(config.project_root.is_some()),
            absolute_floor,
            floor_rules,
            shell_write_floor,
            grants: Vec::new(),
            shell_write_allow: config
                .sandbox
                .write_allow
                .iter()
                .map(|path| expand_home(path, home))
                .collect(),
            private_store_carveouts: private_store_carveouts
                .into_iter()
                .map(|path| policy_path(&path))
                .collect(),
        }
    }

    /// Installs the grants a scope's policy holds: the ones stored for the scope,
    /// plus the built-in Rust library read grants when the scope is a live
    /// worker. Every policy built for a scope comes through here, so the
    /// in-process tool checks and the subc consent and launch checks cannot
    /// disagree about what a worker may read.
    pub(crate) fn install_scope_grants(&mut self, stored: Vec<AccessGrant>, worker: bool) {
        self.install_scope_grants_from(stored, worker, WorkerRustSources::daemon());
    }

    fn install_scope_grants_from(
        &mut self,
        stored: Vec<AccessGrant>,
        worker: bool,
        rust_sources: &WorkerRustSources,
    ) {
        self.grants = stored;
        if worker {
            self.grants.extend(rust_sources.grants());
        }
    }

    pub fn decide(
        &self,
        operation: AccessOperation,
        path: &Path,
        owned_artifact: bool,
    ) -> AccessDecision {
        if self
            .floor_for_path(operation, path, owned_artifact)
            .is_some()
        {
            return AccessDecision::Deny(AccessDenial::Floor);
        }
        let exceptions = match operation {
            AccessOperation::Read => &self.private_stores.read_allow,
            AccessOperation::Write => &self.private_stores.write_allow,
        };
        if !self.standalone_temp_guard_roots.is_empty() {
            let managed = self
                .standalone_managed_temp_roots
                .iter()
                .any(|root| path.starts_with(root))
                || crate::task_temp_reads::is_scratch(path);
            if managed && !self.standalone_owned_scratch {
                return AccessDecision::Deny(AccessDenial::UnsafeTemp);
            }
            if self
                .standalone_temp_guard_roots
                .iter()
                .any(|root| path.starts_with(root))
                && !crate::bounded_io::run(path, None, |path| Ok(temp::safe_file(&path)))
                    .unwrap_or(false)
            {
                return AccessDecision::Deny(AccessDenial::UnsafeTemp);
            }
        }
        let inside = self.roots.iter().any(|root| path.starts_with(root));
        if self.mode == ApprovalMode::ReadOnly && operation == AccessOperation::Write && !inside {
            return AccessDecision::Deny(AccessDenial::ReadOnly);
        }
        // A linked worktree's verified main checkout is readable without an
        // ask, but it is not one of the route's roots: writes there ask like
        // any other write outside every root.
        let main_checkout_read = operation == AccessOperation::Read
            && self
                .standalone_main_roots
                .iter()
                .any(|root| path.starts_with(root));
        if inside
            || main_checkout_read
            || self.standalone_owned_scratch
            || (self.outside == AccessDecision::Ask
                && self
                    .standalone_temp_roots
                    .iter()
                    .any(|root| path.starts_with(root)))
            || exceptions.iter().any(|root| path.starts_with(root))
            || (operation == AccessOperation::Read && owned_artifact)
            || self
                .grants
                .iter()
                .any(|grant| grant.covers(operation, path))
        {
            AccessDecision::Allow
        } else {
            match (self.mode, self.outside) {
                (ApprovalMode::FullAccess, AccessDecision::Ask) => AccessDecision::Allow,
                _ => self.outside,
            }
        }
    }

    fn describe_floor(&self, path: &Path) -> PolicyFloor {
        self.floor_rules
            .iter()
            .find(|floor| floor.path == path)
            .cloned()
            .unwrap_or_else(|| PolicyFloor {
                path: path.to_path_buf(),
                // Public floor vectors also accept programmatically supplied denies.
                rule: FloorRule::PolicyFloor,
            })
    }

    #[cfg(unix)]
    pub(crate) fn add_session_store_floor(&mut self, path: PathBuf) {
        self.floor_rules.push(PolicyFloor {
            path: path.clone(),
            rule: FloorRule::SessionStore,
        });
        self.private_stores.deny.push(path);
    }

    fn floor_for_path(
        &self,
        operation: AccessOperation,
        path: &Path,
        owned_artifact: bool,
    ) -> Option<PolicyFloor> {
        if crate::bash_background::registry::is_task_environment(path) {
            return Some(PolicyFloor {
                path: path.to_path_buf(),
                rule: FloorRule::TaskEnvironment,
            });
        }
        if let Some(floor) = self
            .absolute_floor
            .iter()
            .find(|floor| floor_contains(path, floor))
        {
            return Some(self.describe_floor(floor));
        }
        // Project roots and capability grants cannot admit AFT's SQLite files:
        // closing a second descriptor releases the process's SQLite locks.
        // Ownership-checked task output keeps its separate, non-SQLite read path.
        if !(operation == AccessOperation::Read && owned_artifact) {
            if let Some(floor) = self.floor_rules.iter().find(|floor| {
                matches!(floor.rule, FloorRule::AftStorage) && floor_contains(path, &floor.path)
            }) {
                return Some(floor.clone());
            }
        }
        let exceptions = match operation {
            AccessOperation::Read => &self.private_stores.read_allow,
            AccessOperation::Write => &self.private_stores.write_allow,
        };
        if !exceptions.iter().any(|root| path.starts_with(root))
            && !(operation == AccessOperation::Read && owned_artifact)
            && !self
                .private_store_carveouts
                .iter()
                .any(|carveout| path.starts_with(carveout))
        {
            return self
                .private_stores
                .deny
                .iter()
                .find(|floor| floor_contains(path, floor))
                .map(|floor| self.describe_floor(floor));
        }
        None
    }

    /// Report the same floor that rejects a capability, including a protected
    /// descendant of a subtree. Provenance is recorded when the floor is built.
    pub(crate) fn capability_floor(
        &self,
        operation: AccessOperation,
        kind: GrantKind,
        path: &Path,
    ) -> Option<PolicyFloor> {
        if kind == GrantKind::Subtree {
            for floor in self.absolute_floor.iter().chain(&self.private_stores.deny) {
                if floor_contains(floor, path) {
                    if let Some(matched) = self.floor_for_path(operation, floor, false) {
                        return Some(matched);
                    }
                }
            }
        }
        self.floor_for_path(operation, path, false)
    }

    /// A tree replacement/unlink must not indirectly remove a denied descendant.
    /// Exact location checks still preserve final symlinks instead of following them.
    pub fn decide_tree_write(&self, path: &Path) -> AccessDecision {
        if self
            .capability_floor(AccessOperation::Write, GrantKind::Subtree, path)
            .is_some()
        {
            return AccessDecision::Deny(AccessDenial::Floor);
        }
        if !self.standalone_temp_guard_roots.is_empty() {
            if self
                .standalone_temp_guard_roots
                .iter()
                .chain(&self.standalone_temp_roots)
                .any(|root| path == root)
                || self
                    .standalone_managed_temp_roots
                    .iter()
                    .any(|root| root.starts_with(path) && !self.standalone_owned_scratch)
            {
                return AccessDecision::Deny(AccessDenial::UnsafeTemp);
            }
            if self
                .standalone_temp_guard_roots
                .iter()
                .any(|root| path.starts_with(root))
            {
                let managed = self.standalone_managed_temp_roots.clone();
                let owned = self.standalone_owned_scratch;
                if !crate::bounded_io::run(path, None, move |path| {
                    Ok(temp::safe_tree(&path, &managed, owned))
                })
                .unwrap_or(false)
                {
                    return AccessDecision::Deny(AccessDenial::UnsafeTemp);
                }
            }
        }
        self.decide(AccessOperation::Write, path, false)
    }

    /// A subtree capability includes future descendants, not just the paths that
    /// exist today. A file grant cannot cover it, and any denied descendant wins.
    pub fn decide_capability(
        &self,
        operation: AccessOperation,
        kind: GrantKind,
        path: &Path,
    ) -> AccessDecision {
        if kind == GrantKind::File {
            return self.decide(operation, path, false);
        }
        if self.capability_floor(operation, kind, path).is_some() {
            return AccessDecision::Deny(AccessDenial::Floor);
        }
        let mut tree_policy = self.clone();
        tree_policy
            .grants
            .retain(|grant| grant.kind == GrantKind::Subtree);
        tree_policy.decide(operation, path, false)
    }

    pub fn head_policy(&self, grant: &AccessGrant) -> HeadPolicy {
        if self.decide_capability(grant.operation, grant.kind, &grant.path) == AccessDecision::Allow
        {
            HeadPolicy::Allow
        } else {
            HeadPolicy::NotHeld
        }
    }

    /// Select held shell capabilities without widening file grants to directories.
    /// A subtree must be usable in full; a denied descendant cannot be delegated.
    pub(crate) fn shell_grants(&self, operation: AccessOperation) -> Vec<PathBuf> {
        self.grants
            .iter()
            .filter(|grant| {
                grant.covers(operation, &grant.path)
                    && match operation {
                        AccessOperation::Write => {
                            grant.kind == GrantKind::Subtree
                                && grant.path.is_dir()
                                && self.decide_tree_write(&grant.path) == AccessDecision::Allow
                        }
                        AccessOperation::Read => {
                            (match grant.kind {
                                GrantKind::Subtree => grant.path.is_dir(),
                                GrantKind::File => grant.path.is_file(),
                            }) && self.decide_capability(operation, grant.kind, &grant.path)
                                == AccessDecision::Allow
                        }
                    }
            })
            .map(|grant| grant.path.clone())
            .collect()
    }

    /// Compile directory write capabilities and exact read capabilities. An ask
    /// contributes nothing, and a file grant never opens its parent directory.
    pub fn compile_shell(
        &self,
        temp_dir: PathBuf,
        cache_roots: Vec<PathBuf>,
        socket_deny: Vec<PathBuf>,
    ) -> Result<SandboxProfile, SandboxProfileError> {
        let mut writable = self.roots.clone();
        writable.extend(self.shell_write_allow.iter().cloned());
        let write_grants = self.shell_grants(AccessOperation::Write);
        writable.extend(write_grants.iter().cloned());
        let mut profile = SandboxProfile::build(
            writable,
            self.shell_write_floor.clone(),
            Vec::new(),
            Vec::new(),
            self.absolute_floor.clone(),
            socket_deny,
            cache_roots,
            temp_dir.clone(),
        )?;
        profile.data_policy = self.private_stores.clone();
        profile.data_policy.write_allow.extend(write_grants);
        profile
            .data_policy
            .read_allow
            .extend(self.shell_grants(AccessOperation::Read));
        profile.data_policy.read_allow.push(temp_dir.clone());
        profile.data_policy.write_allow.push(temp_dir);
        Ok(profile)
    }
}

fn environment_private_store_floors(
    getenv: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> Vec<PolicyFloor> {
    let mut denies = Vec::new();
    for (variable, suffix, absolute_only, rule) in [
        ("SUBC_CONNECTION_FILE", "", false, FloorRule::SubcConnection),
        (
            "XDG_RUNTIME_DIR",
            "subc-connection.json",
            false,
            FloorRule::SubcConnection,
        ),
        (
            "XDG_DATA_HOME",
            "cortexkit",
            true,
            FloorRule::CortexkitModuleData,
        ),
        (
            "XDG_STATE_HOME",
            "cortexkit",
            true,
            FloorRule::CortexkitModuleState,
        ),
    ] {
        let Some(value) = getenv(variable).filter(|value| !value.is_empty()) else {
            continue;
        };
        let mut path = PathBuf::from(value);
        if absolute_only && !path.is_absolute() {
            continue;
        }
        if !suffix.is_empty() {
            path.push(suffix);
        }
        #[cfg(unix)]
        {
            if !path.is_absolute() {
                match std::env::current_dir() {
                    Ok(cwd) => path = cwd.join(path),
                    Err(error) => {
                        log::warn!("skipping sandbox deny from {variable}: {error}");
                        continue;
                    }
                }
            }
            denies.extend(
                crate::sandbox_profile::resolved_env_deny_paths(variable, path)
                    .into_iter()
                    .map(|path| PolicyFloor { path, rule }),
            );
        }
        #[cfg(not(unix))]
        denies.push(PolicyFloor {
            path: policy_path(&path),
            rule,
        });
    }
    denies
}

#[cfg(all(test, unix))]
fn environment_private_store_denies(
    getenv: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> Vec<PathBuf> {
    environment_private_store_floors(getenv)
        .into_iter()
        .map(|floor| floor.path)
        .collect()
}

#[cfg(not(any(windows, target_os = "macos")))]
fn floor_contains(path: &Path, prefix: &Path) -> bool {
    path.starts_with(prefix)
}

#[cfg(windows)]
fn floor_contains(path: &Path, prefix: &Path) -> bool {
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        #[link_name = "CompareStringOrdinal"]
        fn compare_string_ordinal(
            left: *const u16,
            left_len: i32,
            right: *const u16,
            right_len: i32,
            ignore_case: i32,
        ) -> i32;
    }

    // Windows accepts case aliases even for missing final components. Compare
    // whole components with the OS's ordinal case rules, without following a
    // final symlink or losing non-UTF-8 names through string conversion.
    let path = crate::inspect::job::normalize_path(path);
    let prefix = crate::inspect::job::normalize_path(prefix);
    let mut components = path.components();
    prefix.components().all(|expected| {
        let Some(actual) = components.next() else {
            return false;
        };
        let left = actual.as_os_str().encode_wide().collect::<Vec<_>>();
        let right = expected.as_os_str().encode_wide().collect::<Vec<_>>();
        let (Ok(left_len), Ok(right_len)) = (i32::try_from(left.len()), i32::try_from(right.len()))
        else {
            return false;
        };
        // Both UTF-16 buffers stay alive for the call, and the lengths are checked.
        // CompareStringOrdinal returns CSTR_EQUAL (2); errors conservatively
        // match a protected name. This comparator never grants access.
        unsafe {
            matches!(
                compare_string_ordinal(left.as_ptr(), left_len, right.as_ptr(), right_len, 1),
                0 | 2
            )
        }
    })
}

#[cfg(target_os = "macos")]
fn floor_contains(path: &Path, prefix: &Path) -> bool {
    use std::ffi::c_void;
    use std::os::unix::ffi::OsStrExt;

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        #[link_name = "CFStringCreateWithCharacters"]
        fn create_string(allocator: *const c_void, chars: *const u16, len: isize) -> *const c_void;
        #[link_name = "CFStringCompare"]
        fn compare_strings(left: *const c_void, right: *const c_void, options: usize) -> isize;
        #[link_name = "CFRelease"]
        fn release(value: *const c_void);
    }

    // Protected names include case and Unicode decomposition aliases on macOS.
    // Use these comparisons only to deny access, never to widen roots or grants.
    let mut components = path.components();
    prefix.components().all(|expected| {
        let Some(actual) = components.next() else {
            return false;
        };
        let (Some(left), Some(right)) =
            (actual.as_os_str().to_str(), expected.as_os_str().to_str())
        else {
            return actual
                .as_os_str()
                .as_bytes()
                .eq_ignore_ascii_case(expected.as_os_str().as_bytes());
        };
        let left = left.encode_utf16().collect::<Vec<_>>();
        let right = right.encode_utf16().collect::<Vec<_>>();
        // The buffers live through string creation. Every created CF string is
        // released, and allocation failure conservatively matches a protected name.
        unsafe {
            let left = create_string(std::ptr::null(), left.as_ptr(), left.len() as isize);
            let right = create_string(std::ptr::null(), right.as_ptr(), right.len() as isize);
            // Ignore case and treat different Unicode decompositions of a letter as equal.
            let equal =
                left.is_null() || right.is_null() || compare_strings(left, right, 1 | 16) == 0;
            if !left.is_null() {
                release(left);
            }
            if !right.is_null() {
                release(right);
            }
            equal
        }
    })
}

pub(crate) fn expand_home(path: &Path, home: &Path) -> PathBuf {
    let mut components = path.components();
    if components
        .next()
        .is_some_and(|part| part.as_os_str() == "~")
    {
        components.fold(home.to_path_buf(), |path, part| path.join(part))
    } else {
        path.to_path_buf()
    }
}

/// Identity for policy comparisons only. Tool return paths retain their existing
/// resolution contract, including the unchanged spelling when unrestricted.
pub(crate) fn policy_path(path: &Path) -> PathBuf {
    let bounded = BOUNDED_POLICY_PATHS.with(|slot| slot.borrow().clone());
    if let Some(state) = bounded {
        if state.error.borrow().is_none() {
            *state.progress.lock().unwrap_or_else(|e| e.into_inner()) = path.to_path_buf();
            match crate::bounded_io::run(path, None, |path| Ok(resolve_policy_path(&path))) {
                Ok(path) => return path,
                Err(error) => *state.error.borrow_mut() = Some(error),
            }
        }
        // The enclosing resolution must reject this entire policy. Do not run
        // more filesystem work after its first failure.
        return crate::inspect::job::normalize_path(path);
    }
    resolve_policy_path(path)
}

struct BoundedPolicyPaths {
    progress: std::sync::Arc<std::sync::Mutex<PathBuf>>,
    error: std::cell::RefCell<Option<std::io::Error>>,
}

thread_local! {
    static BOUNDED_POLICY_PATHS: std::cell::RefCell<Option<std::rc::Rc<BoundedPolicyPaths>>> = const { std::cell::RefCell::new(None) };
}

/// Resolve a complete policy with isolated filesystem probes, rejecting all of
/// its derived authority if any probe fails. The caller supplies the I/O budget.
pub(crate) fn with_bounded_policy_paths<T>(
    progress: std::sync::Arc<std::sync::Mutex<PathBuf>>,
    work: impl FnOnce() -> T,
) -> std::io::Result<T> {
    struct Restore(Option<std::rc::Rc<BoundedPolicyPaths>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            BOUNDED_POLICY_PATHS.with(|slot| *slot.borrow_mut() = self.0.take());
        }
    }
    let state = std::rc::Rc::new(BoundedPolicyPaths {
        progress,
        error: Default::default(),
    });
    let _restore = Restore(BOUNDED_POLICY_PATHS.with(|slot| slot.replace(Some(state.clone()))));
    let value = work();
    let error = state.error.borrow_mut().take();
    match error {
        Some(error) => Err(error),
        None => Ok(value),
    }
}

fn resolve_policy_path(path: &Path) -> PathBuf {
    #[cfg(all(test, unix))]
    crate::bounded_io::stall_running_helper_if_requested(path);
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(path)
    };
    let mut ancestor = path.clone();
    let mut tail = Vec::new();
    loop {
        if let Ok(mut canonical) = ancestor.canonicalize() {
            for component in tail.iter().rev() {
                canonical.push(component);
            }
            return crate::inspect::job::normalize_path(&canonical);
        }
        let Some(name) = ancestor.file_name().map(ToOwned::to_owned) else {
            return crate::inspect::job::normalize_path(&path);
        };
        tail.push(name);
        if !ancestor.pop() {
            return crate::inspect::job::normalize_path(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    #[test]
    fn environment_store_denies_resolve_dotdot_and_symlink_targets() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        std::fs::create_dir_all(base.join("target/nested")).unwrap();
        std::os::unix::fs::symlink(base.join("target/nested"), base.join("alias")).unwrap();
        for (variable, suffix) in [
            ("SUBC_CONNECTION_FILE", "connection.json"),
            ("XDG_RUNTIME_DIR", "runtime/subc-connection.json"),
            ("XDG_DATA_HOME", "data/cortexkit"),
            ("XDG_STATE_HOME", "state/cortexkit"),
        ] {
            let leaf = match variable {
                "SUBC_CONNECTION_FILE" => "connection.json",
                "XDG_RUNTIME_DIR" => "runtime",
                "XDG_DATA_HOME" => "data",
                _ => "state",
            };
            let value = base.join("alias/..").join(leaf);
            let expected = base.join("target").join(suffix);
            for exists in [false, true] {
                if exists {
                    std::fs::create_dir_all(expected.parent().unwrap()).unwrap();
                    std::fs::write(&expected, "private").unwrap();
                }
                let denies = super::environment_private_store_denies(|key| {
                    (key == variable).then(|| value.clone().into_os_string())
                });
                assert_eq!(
                    denies,
                    vec![expected.clone()],
                    "{variable}, exists={exists}"
                );
            }
        }
        let denies = super::environment_private_store_denies(|key| {
            (key == "XDG_DATA_HOME").then(|| base.join("alias").into_os_string())
        });
        assert!(denies.contains(&base.join("alias/cortexkit")), "{denies:?}");
        assert!(
            denies.contains(&base.join("target/nested/cortexkit")),
            "{denies:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn environment_store_denies_skip_only_unresolvable_entries() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let link = base.join("loop");
        std::os::unix::fs::symlink(&link, &link).unwrap();
        let denies = super::environment_private_store_denies(|key| match key {
            "XDG_DATA_HOME" => Some(link.clone().into_os_string()),
            "XDG_STATE_HOME" => Some(base.join("state").into_os_string()),
            _ => None,
        });
        assert_eq!(denies, vec![base.join("state/cortexkit")]);
    }

    use super::*;

    #[test]
    fn grants_head_policy_requires_every_descendant_and_operation() {
        let dir = tempfile::tempdir().unwrap();
        let root = policy_path(&dir.path().join("root"));
        let mut policy = AccessPolicy::from_config(
            &Config::default(),
            &dir.path().join("store"),
            vec![root.clone()],
            &dir.path().join("home"),
        );
        let mut grant = AccessGrant {
            path: root.clone(),
            kind: GrantKind::Subtree,
            operation: AccessOperation::Read,
        };
        assert_eq!(policy.head_policy(&grant), HeadPolicy::Allow);
        policy.absolute_floor.push(root.join("future-secret"));
        assert_eq!(
            policy.decide(AccessOperation::Read, &root, false),
            AccessDecision::Allow
        );
        assert_eq!(policy.head_policy(&grant), HeadPolicy::NotHeld);
        grant.operation = AccessOperation::Write;
        assert_eq!(policy.head_policy(&grant), HeadPolicy::NotHeld);
        policy.absolute_floor.clear();
        policy.roots.clear();
        policy.private_stores.read_allow.clear();
        policy.private_stores.write_allow.clear();
        policy.grants.push(AccessGrant {
            operation: AccessOperation::Read,
            ..grant.clone()
        });
        assert_eq!(policy.head_policy(&grant), HeadPolicy::NotHeld);
        grant.operation = AccessOperation::Read;
        assert_eq!(policy.head_policy(&grant), HeadPolicy::Allow);
        grant.path = root.with_file_name("root-sibling");
        assert_eq!(policy.head_policy(&grant), HeadPolicy::NotHeld);
    }

    #[test]
    fn grants_head_policy_does_not_delegate_a_private_store_descendant() {
        let dir = tempfile::tempdir().unwrap();
        let root = policy_path(dir.path());
        let policy = policy(&root.join("own"), &root.join("store"));
        let grant = AccessGrant {
            path: root,
            kind: GrantKind::Subtree,
            operation: AccessOperation::Read,
        };
        assert_eq!(policy.head_policy(&grant), HeadPolicy::NotHeld);
        assert_eq!(
            policy.decide_capability(grant.operation, grant.kind, &grant.path),
            AccessDecision::Deny(AccessDenial::Floor)
        );
    }

    #[test]
    fn grants_file_does_not_hold_a_subtree() {
        let dir = tempfile::tempdir().unwrap();
        let mut policy = policy(&dir.path().join("root"), &dir.path().join("store"));
        let target = policy_path(&dir.path().join("outside"));
        policy.grants.push(AccessGrant {
            path: target.clone(),
            kind: GrantKind::File,
            operation: AccessOperation::Write,
        });
        assert_eq!(policy.decide_tree_write(&target), AccessDecision::Allow);
        assert_eq!(
            policy.decide_capability(AccessOperation::Write, GrantKind::Subtree, &target),
            AccessDecision::Ask
        );
    }

    fn policy(root: &Path, store: &Path) -> AccessPolicy {
        let config = Config::default();
        AccessPolicy::from_config(&config, store, vec![policy_path(root)], &root.join("home"))
    }

    fn assert_read_scope_floor(relative: &str) {
        let dir = tempfile::tempdir().unwrap();
        let home = policy_path(&dir.path().join("home"));
        let storage = home.join(".local/share/cortexkit/aft");
        let target = home.join(relative);
        let mut policy = AccessPolicy::from_config(
            &Config::default(),
            &storage,
            vec![dir.path().join("worktree")],
            &home,
        );
        let grant = AccessGrant {
            path: home,
            kind: GrantKind::Subtree,
            operation: AccessOperation::Read,
        };
        assert!(grant.covers(AccessOperation::Read, &target));
        policy.grants.push(grant);
        assert_eq!(
            policy.decide(AccessOperation::Read, &target, false),
            AccessDecision::Deny(AccessDenial::Floor)
        );
    }

    #[test]
    fn scope_read_grant_cannot_admit_credentials() {
        assert_read_scope_floor(".ssh/id_ed25519");
    }

    #[test]
    fn scope_read_grant_cannot_admit_cortexkit_data() {
        assert_read_scope_floor(".local/share/cortexkit/alfonso/prompts/private.md");
    }

    #[test]
    fn scope_read_grant_cannot_admit_environment_bin() {
        assert_read_scope_floor(".local/share/cortexkit/aft/bash/task/control/environment.bin");
    }

    #[test]
    fn worker_worktrees_leave_the_private_store_floor() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let store = home.join(".local/share/cortexkit/aft");
        let worktree = home.join(".local/share/cortexkit/alfonso/worktrees/pool/bg_x/src/lib.rs");
        let private = home.join(".local/share/cortexkit/alfonso/prompts/brief.md");
        let project = root.path().join("project");
        let policy = AccessPolicy::from_config(
            &Config::default(),
            &store,
            vec![policy_path(&project)],
            &home,
        );
        for operation in [AccessOperation::Read, AccessOperation::Write] {
            // A worker's checkout is ordinary code outside this root: the
            // outside rule decides, never the private-store floor.
            assert_eq!(
                policy.decide(operation, &policy_path(&worktree), false),
                AccessDecision::Ask
            );
            for denied in [&private, &store.join("aft.db")] {
                assert_eq!(
                    policy.decide(operation, &policy_path(denied), false),
                    AccessDecision::Deny(AccessDenial::Floor),
                    "{}",
                    denied.display()
                );
            }
        }
    }

    #[test]
    fn absolute_floor_overrides_full_access_and_covering_grant() {
        let root = tempfile::tempdir().unwrap();
        let mut policy = policy(root.path(), &root.path().join("store"));
        let target = root.path().join("blocked");
        policy.absolute_floor.push(target.clone());
        policy.outside = AccessDecision::Allow;
        policy.mode = ApprovalMode::FullAccess;
        policy.grants.push(AccessGrant {
            path: target.clone(),
            kind: GrantKind::Subtree,
            operation: AccessOperation::Write,
        });
        assert_eq!(
            policy.decide(AccessOperation::Read, &target, true),
            AccessDecision::Deny(AccessDenial::Floor)
        );
        assert_eq!(
            policy.decide(AccessOperation::Write, &target, false),
            AccessDecision::Deny(AccessDenial::Floor)
        );
    }

    #[test]
    fn storage_floor_grants_and_project_roots_cannot_add_carve_outs() {
        let root = tempfile::tempdir().unwrap();
        let outside_dir = tempfile::tempdir().unwrap();
        // Production callers pass canonical policy identities. On macOS the temp
        // directory is reached through the /var -> /private/var link, so use the
        // same identity here.
        let outside = policy_path(outside_dir.path());
        let mut policy = policy(root.path(), &outside);
        policy.outside = AccessDecision::Allow;
        policy.shell_write_allow.push(outside.clone());
        policy.grants.push(AccessGrant {
            path: outside.clone(),
            kind: GrantKind::Subtree,
            operation: AccessOperation::Write,
        });
        assert_eq!(
            policy.decide(AccessOperation::Read, &outside, false),
            AccessDecision::Deny(AccessDenial::Floor)
        );
        assert_eq!(
            policy.decide(AccessOperation::Write, &outside, true),
            AccessDecision::Deny(AccessDenial::Floor)
        );
        assert_eq!(
            policy.decide(AccessOperation::Read, &outside, true),
            AccessDecision::Allow
        );
        policy.private_stores.read_allow.push(outside.clone());
        assert_eq!(
            policy.decide(AccessOperation::Read, &outside, false),
            AccessDecision::Deny(AccessDenial::Floor)
        );
        assert_eq!(
            policy.decide(AccessOperation::Write, &outside, false),
            AccessDecision::Deny(AccessDenial::Floor)
        );
    }

    #[test]
    fn shell_compiler_excludes_file_grants_and_unresolved_asks() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let file = outside.path().join("approved-file");
        std::fs::write(&file, "file").unwrap();
        let directory = outside.path().join("approved-directory");
        std::fs::create_dir(&directory).unwrap();
        let file_kind_directory = outside.path().join("file-kind-directory");
        std::fs::create_dir(&file_kind_directory).unwrap();
        let denied_directory = outside.path().join("denied-directory");
        std::fs::create_dir(&denied_directory).unwrap();
        let temp = root.path().join("temp");
        std::fs::create_dir(&temp).unwrap();
        let mut policy = policy(root.path(), &root.path().join("store"));
        policy.absolute_floor.push(denied_directory.join("floor"));
        policy.grants = vec![
            AccessGrant {
                path: file_kind_directory,
                kind: GrantKind::File,
                operation: AccessOperation::Write,
            },
            AccessGrant {
                path: denied_directory,
                kind: GrantKind::Subtree,
                operation: AccessOperation::Write,
            },
            AccessGrant {
                path: file.clone(),
                kind: GrantKind::File,
                operation: AccessOperation::Write,
            },
            AccessGrant {
                path: outside.path().join("future-file"),
                kind: GrantKind::File,
                operation: AccessOperation::Write,
            },
            AccessGrant {
                path: directory.clone(),
                kind: GrantKind::Subtree,
                operation: AccessOperation::Write,
            },
            AccessGrant {
                path: outside.path().join("missing-directory"),
                kind: GrantKind::Subtree,
                operation: AccessOperation::Write,
            },
        ];
        assert_eq!(
            policy.decide(AccessOperation::Write, &file, false),
            AccessDecision::Allow
        );
        assert_eq!(
            policy.decide(AccessOperation::Read, &file, false),
            AccessDecision::Allow
        );
        assert_eq!(
            policy.decide(
                AccessOperation::Write,
                &outside.path().join("sibling"),
                false
            ),
            AccessDecision::Ask
        );
        let profile = policy.compile_shell(temp, Vec::new(), Vec::new()).unwrap();
        assert_eq!(
            profile.writable_roots,
            vec![
                root.path().canonicalize().unwrap(),
                directory.canonicalize().unwrap()
            ]
        );
        assert!(!profile
            .write_allow_roots()
            .iter()
            .any(|path| file.starts_with(path)));
    }

    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn floor_matching_uses_native_case_insensitive_components() {
        let root = tempfile::tempdir().unwrap();
        let mut policy = policy(root.path(), &root.path().join("store"));
        policy.outside = AccessDecision::Allow;
        policy
            .absolute_floor
            .push(root.path().join("private-Ω/Secret.txt"));
        let alias = root.path().join("PRIVATE-ω/secret.TXT");
        assert_eq!(
            policy.decide(AccessOperation::Write, &alias, false),
            AccessDecision::Deny(AccessDenial::Floor)
        );
        assert_eq!(
            policy.decide(AccessOperation::Read, &alias, true),
            AccessDecision::Deny(AccessDenial::Floor)
        );
        assert_eq!(
            policy.decide(
                AccessOperation::Write,
                &root.path().join("PRIVATE-ω/secret.TXT-sibling"),
                false
            ),
            AccessDecision::Allow
        );
    }

    #[test]
    fn user_read_deny_does_not_become_a_shell_write_floor() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("worktree");
        let denied = root.join("private");
        let temp = fixture.path().join("task-temp");
        std::fs::create_dir_all(&denied).unwrap();
        std::fs::create_dir(&temp).unwrap();
        let mut config = Config::default();
        config.sandbox.read_deny = vec![denied.clone()];
        let policy = AccessPolicy::from_config(
            &config,
            &fixture.path().join("store"),
            vec![root.clone()],
            &fixture.path().join("home"),
        );
        let profile = policy.compile_shell(temp, Vec::new(), Vec::new()).unwrap();
        let denied = denied.canonicalize().unwrap();
        assert!(profile.read_deny.contains(&denied));
        assert!(
            !profile.write_deny.contains(&denied),
            "user read_deny must not reject an enclosing writable project"
        );
        assert_eq!(profile.writable_roots, vec![root.canonicalize().unwrap()]);
    }

    #[test]
    fn subtree_matching_uses_whole_components() {
        let root = tempfile::tempdir().unwrap();
        let mut policy = policy(&root.path().join("own"), &root.path().join("store"));
        let granted = root.path().join("a/b");
        policy.grants.push(AccessGrant {
            path: granted.clone(),
            kind: GrantKind::Subtree,
            operation: AccessOperation::Read,
        });
        assert_eq!(
            policy.decide(AccessOperation::Read, &granted.join("c"), false),
            AccessDecision::Allow
        );
        assert_eq!(
            policy.decide(AccessOperation::Write, &granted.join("c"), false),
            AccessDecision::Ask
        );
        assert_eq!(
            policy.decide(AccessOperation::Read, &root.path().join("a/bc"), false),
            AccessDecision::Ask
        );
    }
}

#[cfg(test)]
#[path = "access_policy_worker_rust_tests.rs"]
mod worker_rust_tests;

#[cfg(test)]
#[path = "access_policy_audit2_tests.rs"]
mod audit2_tests;

#[cfg(test)]
#[path = "access_policy_outside_roots_tests.rs"]
mod outside_roots_tests;
