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

/// Who answers an `ask` decision for the call running on this thread.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum AskRoute {
    /// `ask` is treated as allow for standalone or unattested calls, which
    /// have no Cingulate consent route. Restricted configurations turn outside
    /// paths into deny, so they never reach an `ask`.
    #[default]
    Lowered,
    /// An attested agent scope: an `ask` refuses with `access_ask_required`
    /// so tool-provider dispatch can ask the Cingulate consent service. `once`
    /// holds the capabilities approved by `allow_once` for this dispatch.
    Consent {
        once: Vec<AccessGrant>,
        /// Enables missing-path suggestions and filesystem-root refusals for
        /// workers only. Primary agents in ask mode also use consent.
        worker: bool,
    },
}

/// Internal refusal code for an `ask` that must go to Cingulate. The subc
/// layer turns it into a held call; it never reaches the agent as-is.
pub(crate) const ACCESS_ASK_REQUIRED: &str = "access_ask_required";

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

#[derive(Debug, Clone)]
pub struct AccessPolicy {
    pub roots: Vec<PathBuf>,
    pub mode: ApprovalMode,
    pub outside: AccessDecision,
    pub absolute_floor: Vec<PathBuf>,
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
}

impl AccessPolicy {
    /// Snapshot the route configuration. Unrestricted outside access is represented
    /// as Ask and lowered to the existing behavior by the in-process caller.
    pub fn from_config(config: &Config, storage: &Path, roots: Vec<PathBuf>, home: &Path) -> Self {
        let roots = roots
            .into_iter()
            .map(|root| crate::inspect::job::normalize_path(&root))
            .collect::<Vec<_>>();
        let shell_write_floor = [
            ".ssh",
            ".aws",
            ".gnupg",
            ".config/gcloud",
            ".azure",
            ".config/cortexkit",
        ]
        .into_iter()
        .map(|path| home.join(path))
        .map(|path| policy_path(&path))
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
                .map(|path| policy_path(&path)),
            )
            .collect::<Vec<_>>();
        let mut absolute_floor = shell_write_floor.clone();
        absolute_floor.extend(
            config
                .sandbox
                .read_deny
                .iter()
                .map(|path| policy_path(&expand_home(path, home))),
        );
        let mut deny = vec![
            home.join(".local/share/cortexkit"),
            home.join(".local/state/cortexkit"),
            storage.to_path_buf(),
            home.join(".local/share/cortexkit/run/subc-connection.json"),
        ];
        if let Some(path) = &config.semantic.subc_connection_file {
            let path = expand_home(path, home);
            deny.push(if path.is_absolute() {
                path
            } else {
                home.join(path)
            });
        }
        if let Some(path) = crate::environment::non_empty_os_var("SUBC_CONNECTION_FILE") {
            deny.push(PathBuf::from(path));
        }
        if let Some(path) = crate::environment::non_empty_os_var("XDG_RUNTIME_DIR") {
            deny.push(PathBuf::from(path).join("subc-connection.json"));
        }
        for variable in ["XDG_DATA_HOME", "XDG_STATE_HOME"] {
            if let Some(path) = crate::environment::non_empty_os_var(variable) {
                let path = PathBuf::from(path);
                if path.is_absolute() {
                    deny.push(path.join("cortexkit"));
                }
            }
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
            mode: ApprovalMode::Ask,
            private_stores: SandboxDataPolicy {
                deny: deny.into_iter().map(|path| policy_path(&path)).collect(),
                read_allow,
                write_allow: roots.clone(),
                read_metadata_allow: Vec::new(),
            },
            roots,
            outside: if config.restrict_to_project_root && config.project_root.is_some() {
                AccessDecision::Deny(AccessDenial::OutsideRoots)
            } else {
                AccessDecision::Ask
            },
            absolute_floor,
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

    pub fn decide(
        &self,
        operation: AccessOperation,
        path: &Path,
        owned_artifact: bool,
    ) -> AccessDecision {
        if crate::bash_background::registry::is_task_environment(path) {
            return AccessDecision::Deny(AccessDenial::Floor);
        }
        if self
            .absolute_floor
            .iter()
            .any(|floor| floor_contains(path, floor))
        {
            return AccessDecision::Deny(AccessDenial::Floor);
        }
        let exceptions = match operation {
            AccessOperation::Read => &self.private_stores.read_allow,
            AccessOperation::Write => &self.private_stores.write_allow,
        };
        if self
            .private_stores
            .deny
            .iter()
            .any(|floor| floor_contains(path, floor))
            && !exceptions.iter().any(|root| path.starts_with(root))
            && !(operation == AccessOperation::Read && owned_artifact)
            && !self
                .private_store_carveouts
                .iter()
                .any(|carveout| path.starts_with(carveout))
        {
            return AccessDecision::Deny(AccessDenial::Floor);
        }
        let inside = self.roots.iter().any(|root| path.starts_with(root));
        if self.mode == ApprovalMode::ReadOnly && operation == AccessOperation::Write && !inside {
            return AccessDecision::Deny(AccessDenial::ReadOnly);
        }
        if inside
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

    /// A tree replacement/unlink must not indirectly remove a denied descendant.
    /// Exact location checks still preserve final symlinks instead of following them.
    pub fn decide_tree_write(&self, path: &Path) -> AccessDecision {
        for floor in self.absolute_floor.iter().chain(&self.private_stores.deny) {
            if floor_contains(floor, path)
                && self.decide(AccessOperation::Write, floor, false)
                    == AccessDecision::Deny(AccessDenial::Floor)
            {
                return AccessDecision::Deny(AccessDenial::Floor);
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
        for floor in self.absolute_floor.iter().chain(&self.private_stores.deny) {
            if floor_contains(floor, path)
                && self.decide(operation, floor, false) == AccessDecision::Deny(AccessDenial::Floor)
            {
                return AccessDecision::Deny(AccessDenial::Floor);
            }
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

    /// Compile only directory write capabilities. An ask contributes nothing,
    /// and a file grant must never widen into a writable parent directory.
    pub fn compile_shell(
        &self,
        temp_dir: PathBuf,
        cache_roots: Vec<PathBuf>,
        socket_deny: Vec<PathBuf>,
    ) -> Result<SandboxProfile, SandboxProfileError> {
        let mut writable = self.roots.clone();
        writable.extend(self.shell_write_allow.iter().cloned());
        writable.extend(
            self.grants
                .iter()
                .filter(|grant| {
                    grant.kind == GrantKind::Subtree
                        && grant.operation == AccessOperation::Write
                        && grant.path.is_dir()
                        && self.decide_tree_write(&grant.path) == AccessDecision::Allow
                })
                .map(|grant| grant.path.clone()),
        );
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
        profile.data_policy.read_allow.push(temp_dir.clone());
        profile.data_policy.write_allow.push(temp_dir);
        Ok(profile)
    }
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
    fn private_store_grants_cannot_add_carve_outs() {
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
            AccessDecision::Allow
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
