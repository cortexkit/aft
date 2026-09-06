//! Background bash task management: spawning detached tasks, the watchdog that
//! reaps them, output buffering/compression, and on-disk persistence so tasks
//! survive a bridge restart.

pub mod buffer;
pub mod output;
pub mod persistence;
pub mod process;
pub mod pty_process;
pub mod pty_runtime;
pub mod registry;
pub mod watchdog;
pub mod watches;

use crate::bash_permissions::PermissionAsk;
use crate::context::AppContext;
use crate::protocol::Response;
#[cfg(unix)]
use crate::sandbox_spawn::native_sandbox_enforced;
use crate::sandbox_spawn::{
    current_authenticated_principal, resolve_sandbox_spawn, HostEscalationAttempt,
    RequestedSandboxTier, SandboxTaskKind,
};
use persistence::BgMode;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

pub use registry::{BgCompletion, BgTaskHealthCounts, BgTaskRegistry};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BashShell {
    #[default]
    Bash,
    Powershell,
}

impl BashShell {
    pub(crate) fn is_powershell(self) -> bool {
        matches!(self, Self::Powershell)
    }

    pub(crate) fn command_text(self, command: &str) -> String {
        if self.is_powershell() {
            // Match Pi's optional tool: both .NET and PowerShell's pipeline use
            // UTF-8 before user code runs, so redirected native output remains
            // readable across macOS, Linux, and Windows.
            format!(
                "[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false); $OutputEncoding = [Console]::OutputEncoding;\n{command}"
            )
        } else {
            command.to_string()
        }
    }
}

fn resolve_powershell_path_with(
    lookup: impl FnOnce(&str) -> Option<PathBuf>,
) -> Result<PathBuf, String> {
    #[cfg(windows)]
    let candidate = "pwsh.exe";
    #[cfg(not(windows))]
    let candidate = "pwsh";
    lookup(candidate).ok_or_else(|| {
        "PowerShell (pwsh) is not installed or is not on PATH. Install PowerShell 7+: https://aka.ms/powershell"
            .to_string()
    })
}

pub(crate) fn resolve_shell_path(pty: bool, shell: BashShell) -> Result<PathBuf, String> {
    if shell.is_powershell() {
        return resolve_powershell_path_with(|candidate| which::which(candidate).ok());
    }

    #[cfg(unix)]
    {
        Ok(if pty {
            pty_process::resolve_posix_shell()
        } else {
            registry::resolve_posix_shell()
        })
    }
    #[cfg(windows)]
    {
        let _ = pty;
        Ok(PathBuf::from("cmd.exe"))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BgTaskInfo {
    pub task_id: String,
    pub status: BgTaskStatus,
    pub command: String,
    pub mode: BgMode,
    pub started_at: u64,
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BgTaskStatus {
    Starting,
    Running,
    Killing,
    Completed,
    Failed,
    Killed,
    TimedOut,
    FateUnknown,
}

impl BgTaskStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            BgTaskStatus::Completed
                | BgTaskStatus::Failed
                | BgTaskStatus::Killed
                | BgTaskStatus::TimedOut
                | BgTaskStatus::FateUnknown
        )
    }
}

/// Spawn a bash command in the background. Returns a task_id immediately.
#[allow(clippy::too_many_arguments)]
pub fn spawn(
    request_id: &str,
    session_id: &str,
    command: &str,
    shell: BashShell,
    shell_path: PathBuf,
    workdir: Option<PathBuf>,
    env: Option<HashMap<String, String>>,
    timeout_ms: Option<u64>,
    ctx: &AppContext,
    require_background_flag: bool,
    notify_on_completion: bool,
    compressed: bool,
    pty: bool,
    pty_rows: u16,
    pty_cols: u16,
    scanner_report: Vec<PermissionAsk>,
    host_escalation: Option<HostEscalationAttempt>,
) -> Response {
    if require_background_flag && !ctx.config().experimental_bash_background {
        return Response::error(
            request_id,
            "feature_disabled",
            "background bash is disabled; set `bash: { background: true }` (or `bash: true`) in aft.jsonc",
        );
    }

    let workdir = workdir.unwrap_or_else(|| {
        ctx.config().project_root.clone().unwrap_or_else(|| {
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
        })
    });
    let storage_dir = task_storage_dir(ctx);
    let max_running = ctx.config().max_background_bash_tasks;
    let timeout = timeout_ms.map(Duration::from_millis);
    let project_root = ctx
        .config()
        .project_root
        .clone()
        .or_else(|| std::env::current_dir().ok())
        .and_then(|path| std::fs::canonicalize(&path).ok().or(Some(path)));

    let mut env = env.unwrap_or_default();
    let config = ctx.config();
    let child_storage_root = self::storage_dir(config.storage_dir.as_deref());
    if let Err(error) =
        crate::agent_child_env::inject(config.as_ref(), &child_storage_root, &mut env)
    {
        return Response::error(request_id, "child_environment_unavailable", error);
    }
    let task_kind = if pty {
        SandboxTaskKind::BashPty
    } else if require_background_flag {
        SandboxTaskKind::BashBackground
    } else {
        SandboxTaskKind::BashForeground
    };
    let principal = current_authenticated_principal();
    let requested_tier = if host_escalation.is_some() {
        RequestedSandboxTier::Host
    } else if ctx.config().sandbox.enabled {
        RequestedSandboxTier::Native
    } else {
        RequestedSandboxTier::Disabled
    };
    let session_dir = persistence::session_tasks_dir(&storage_dir, session_id);
    #[cfg(unix)]
    let (spawn_plan, unregistered_task) = if native_sandbox_enforced(ctx, &principal)
        && host_escalation.is_none()
    {
        let task = match persistence::allocate_task_layout(&storage_dir, session_id) {
            Ok(task) => task,
            Err(error) => {
                return Response::error(
                    request_id,
                    "sandbox_unavailable",
                    format!(
                        "native sandbox failed to create the task artifact directory: {error}; set sandbox.enabled=false to disable native sandboxing"
                    ),
                );
            }
        };
        let plan = resolve_sandbox_spawn(
            ctx,
            &principal,
            requested_tier,
            task_kind,
            &task.paths.io_dir,
            None,
        );
        if plan.refusal_code().is_some() {
            (plan, Some(task))
        } else {
            let root = project_root.as_deref().unwrap_or(&workdir);
            let environment = crate::sandbox_spawn::approved_environment_for_plan(&plan, &env);
            match crate::sandbox_spawn::prepare_task_payload(
                &task,
                command.as_bytes(),
                root,
                &workdir,
                &principal,
                &shell_path,
                &environment,
            ) {
                Ok(prepared) => (plan.with_prepared_task(prepared), Some(task)),
                Err(error) => {
                    let _ = persistence::delete_resolved_task(&task);
                    return Response::error(
                        request_id,
                        "sandbox_unavailable",
                        format!("native sandbox failed to materialize task payload: {error}"),
                    );
                }
            }
        }
    } else {
        (
            resolve_sandbox_spawn(
                ctx,
                &principal,
                requested_tier,
                task_kind,
                &session_dir,
                host_escalation.as_ref(),
            ),
            None,
        )
    };
    #[cfg(not(unix))]
    let spawn_plan = resolve_sandbox_spawn(
        ctx,
        &principal,
        requested_tier,
        task_kind,
        &session_dir,
        host_escalation.as_ref(),
    );
    if let Some(code) = spawn_plan.refusal_code() {
        #[cfg(unix)]
        if let Some(task) = unregistered_task.as_ref() {
            let _ = persistence::delete_resolved_task(task);
        }
        let message = spawn_plan
            .refusal_message()
            .unwrap_or("bash process creation refused by sandbox policy");
        return match spawn_plan.refusal_mismatch_class() {
            Some(class) => Response::error_with_data(
                request_id,
                code,
                message,
                json!({ "mismatch_class": class }),
            ),
            None => Response::error(request_id, code, message),
        };
    }

    let cleanup_plan = spawn_plan.clone();
    let spawn_result = if pty {
        ctx.bash_background().spawn_pty_with_shell(
            spawn_plan,
            command,
            shell,
            shell_path,
            session_id.to_string(),
            workdir,
            env,
            timeout,
            storage_dir,
            max_running,
            notify_on_completion,
            compressed,
            project_root,
            pty_rows,
            pty_cols,
        )
    } else {
        ctx.bash_background().spawn_with_shell(
            spawn_plan,
            command,
            shell,
            shell_path,
            session_id.to_string(),
            workdir,
            env,
            timeout,
            storage_dir,
            max_running,
            notify_on_completion,
            compressed,
            project_root,
        )
    };

    match spawn_result {
        Ok(task_id) => {
            if let Err(error) =
                ctx.bash_background()
                    .record_scanner_report(&task_id, session_id, scanner_report)
            {
                crate::slog_warn!("{error}");
            }
            Response::success(
                request_id,
                json!({
                    "task_id": task_id,
                    "status": BgTaskStatus::Running,
                    "mode": if pty { "pty" } else { "pipes" },
                }),
            )
        }
        Err(message) if message.contains("limit exceeded") => {
            cleanup_plan.cleanup_unspawned();
            #[cfg(unix)]
            if let Some(task) = unregistered_task.as_ref() {
                let _ = persistence::delete_resolved_task(task);
            }
            Response::error(request_id, "background_task_limit_exceeded", message)
        }
        Err(message) => {
            cleanup_plan.cleanup_unspawned();
            #[cfg(unix)]
            if let Some(task) = unregistered_task.as_ref() {
                let _ = persistence::delete_resolved_task(task);
            }
            if cleanup_plan.is_native_launcher() {
                Response::error(
                    request_id,
                    "sandbox_unavailable",
                    format!(
                        "native sandbox failed before command execution: {message}; set sandbox.enabled=false to disable native sandboxing"
                    ),
                )
            } else {
                Response::error(request_id, "execution_failed", message)
            }
        }
    }
}

pub(crate) fn task_storage_dir(ctx: &AppContext) -> PathBuf {
    let config = ctx.config();
    let root = storage_dir(config.storage_dir.as_deref());
    config
        .harness
        .as_ref()
        .map(|harness| root.join(harness.storage_segment()))
        .unwrap_or(root)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StoragePlatform {
    Windows,
    Other,
}

impl StoragePlatform {
    fn current() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else {
            Self::Other
        }
    }
}

/// Resolve the process-state storage root exactly once for every Rust entry point.
/// The environment override is checked here so it wins over a stale plugin wire
/// value, while both plugin-less fallback and plugin-injected paths share one root.
///
/// Last re-derived 2026-09-06 against subconscious d5e09914b0791a66f2a5a00a9bb3422860ade95e:
/// compare the ordered variables, platform gates, and empty-value guards below with
/// `subc-core/src/daemon_config.rs::default_data_home`, resolve its named constants,
/// then preserve the documented Windows cache-class divergence.
pub fn storage_dir(configured: Option<&std::path::Path>) -> PathBuf {
    let lookup = |name: &str| std::env::var_os(name);
    let fallback_home = std::env::home_dir();
    let current_dir = std::env::current_dir().ok();
    storage_dir_from(
        configured,
        &lookup,
        StoragePlatform::current(),
        fallback_home.as_deref(),
        current_dir.as_deref(),
    )
}

fn storage_dir_from(
    configured: Option<&std::path::Path>,
    lookup: &impl Fn(&str) -> Option<std::ffi::OsString>,
    platform: StoragePlatform,
    fallback_home: Option<&std::path::Path>,
    current_dir: Option<&std::path::Path>,
) -> PathBuf {
    let resolve = |path: &std::path::Path| {
        resolve_storage_path_from(path, lookup, platform, fallback_home, current_dir)
    };

    if let Some(dir) = non_empty_env_path_from(lookup, "AFT_STORAGE_DIR") {
        return resolve(&dir);
    }
    if let Some(dir) = configured.filter(|path| !path.as_os_str().is_empty()) {
        // Explicit process-state paths are already caller-owned. Preserve their
        // spelling so every downstream read/write uses the exact configured root.
        return dir.to_path_buf();
    }
    // AFT_CACHE_DIR predates AFT_STORAGE_DIR as the storage sandbox lever. It
    // remains above the shared data-home ladder so old isolated invocations do
    // not escape into the operator's data root.
    if let Some(dir) = non_empty_env_path_from(lookup, "AFT_CACHE_DIR") {
        return resolve(&dir).join("aft");
    }

    resolve(&cortexkit_data_root_from(lookup, platform))
        .join("cortexkit")
        .join("aft")
}

fn non_empty_env_path_from(
    lookup: &impl Fn(&str) -> Option<std::ffi::OsString>,
    name: &str,
) -> Option<PathBuf> {
    lookup(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn storage_home_dir_from(
    lookup: &impl Fn(&str) -> Option<std::ffi::OsString>,
    platform: StoragePlatform,
    fallback_home: Option<&std::path::Path>,
) -> Option<PathBuf> {
    let configured = if platform == StoragePlatform::Windows {
        non_empty_env_path_from(lookup, "USERPROFILE")
            .or_else(|| non_empty_env_path_from(lookup, "HOME"))
    } else {
        non_empty_env_path_from(lookup, "HOME")
            .or_else(|| non_empty_env_path_from(lookup, "USERPROFILE"))
    };
    configured.or_else(|| fallback_home.map(PathBuf::from))
}

fn cortexkit_data_root_from(
    lookup: &impl Fn(&str) -> Option<std::ffi::OsString>,
    platform: StoragePlatform,
) -> PathBuf {
    if let Some(dir) = non_empty_env_path_from(lookup, "XDG_DATA_HOME") {
        return dir;
    }
    if platform == StoragePlatform::Windows {
        // AFT stores indexes, backups, and checkpoints here.
        // cache-class storage; stable for existing installs.
        // Do not move this shipped ladder to Roaming.
        if let Some(dir) = non_empty_env_path_from(lookup, "LOCALAPPDATA") {
            return dir;
        }
        if let Some(home) = non_empty_env_path_from(lookup, "USERPROFILE") {
            return home.join("AppData").join("Local");
        }
    }
    if let Some(home) = non_empty_env_path_from(lookup, "HOME") {
        return home.join(".local").join("share");
    }
    PathBuf::from(".local").join("share")
}

fn resolve_storage_path_from(
    path: &std::path::Path,
    lookup: &impl Fn(&str) -> Option<std::ffi::OsString>,
    platform: StoragePlatform,
    fallback_home: Option<&std::path::Path>,
    current_dir: Option<&std::path::Path>,
) -> PathBuf {
    let storage_home = || storage_home_dir_from(lookup, platform, fallback_home);
    let expanded = if path == std::path::Path::new("~") {
        storage_home().unwrap_or_else(|| path.to_path_buf())
    } else if let Some(raw) = path.to_str() {
        if raw.starts_with("~/") || raw.starts_with("~\\") {
            storage_home()
                .map(|home| home.join(&raw[2..]))
                .unwrap_or_else(|| path.to_path_buf())
        } else {
            path.to_path_buf()
        }
    } else {
        path.to_path_buf()
    };
    let absolute = if expanded.is_absolute() {
        expanded
    } else if let Some(current_dir) = current_dir {
        current_dir.join(expanded)
    } else {
        expanded
    };
    normalize_absolute_path(&absolute)
}

fn normalize_absolute_path(path: &std::path::Path) -> PathBuf {
    use std::path::Component;

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    normalized.push(component.as_os_str());
                }
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

pub fn repair_legacy_root_tasks(storage_root: &std::path::Path, harness: crate::harness::Harness) {
    let root_tasks = storage_root.join("bash-tasks");
    if !dir_has_entries(&root_tasks) {
        return;
    }

    let harness_tasks = storage_root
        .join(harness.storage_segment())
        .join("bash-tasks");
    if dir_has_entries(&harness_tasks) {
        return;
    }
    if let Some(parent) = harness_tasks.parent() {
        if let Err(error) = std::fs::create_dir_all(parent) {
            crate::slog_warn!(
                "failed to create harness bash task dir {}: {}",
                parent.display(),
                error
            );
            return;
        }
    }
    if harness_tasks.exists() {
        let _ = std::fs::remove_dir(&harness_tasks);
    }

    match std::fs::rename(&root_tasks, &harness_tasks) {
        Ok(()) => crate::slog_info!(
            "moved legacy root bash tasks into harness namespace: {}",
            harness_tasks.display()
        ),
        Err(error) => {
            crate::slog_warn!(
                "failed to move legacy root bash tasks into {}: {}; trying child merge",
                harness_tasks.display(),
                error
            );
            if std::fs::create_dir_all(&harness_tasks).is_err() {
                return;
            }
            if let Ok(entries) = std::fs::read_dir(&root_tasks) {
                for entry in entries.flatten() {
                    let source = entry.path();
                    let target = harness_tasks.join(entry.file_name());
                    if !target.exists() {
                        let _ = std::fs::rename(source, target);
                    }
                }
            }
            let _ = std::fs::remove_dir(&root_tasks);
        }
    }
}

fn dir_has_entries(path: &std::path::Path) -> bool {
    std::fs::read_dir(path)
        .map(|mut entries| entries.next().is_some())
        .unwrap_or(false)
}

#[cfg(test)]
mod storage_root_tests {
    use std::collections::HashMap;
    use std::ffi::OsString;
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::path::{Path, PathBuf};

    struct NonPanickingCleanup<F: FnOnce()> {
        cleanup: Option<F>,
    }

    impl<F: FnOnce()> NonPanickingCleanup<F> {
        fn new(cleanup: F) -> Self {
            Self {
                cleanup: Some(cleanup),
            }
        }
    }

    impl<F: FnOnce()> Drop for NonPanickingCleanup<F> {
        fn drop(&mut self) {
            let Some(cleanup) = self.cleanup.take() else {
                return;
            };
            // A cleanup panic while the test is already unwinding aborts the whole
            // libtest process, so cleanup failures must remain contained here.
            let _ = catch_unwind(AssertUnwindSafe(cleanup));
        }
    }

    fn resolve_storage_fixture(
        env: &HashMap<&str, OsString>,
        configured: Option<&Path>,
        platform: super::StoragePlatform,
        fallback_home: Option<&Path>,
        current_dir: Option<&Path>,
    ) -> PathBuf {
        super::storage_dir_from(
            configured,
            &|name| env.get(name).cloned(),
            platform,
            fallback_home,
            current_dir,
        )
    }

    #[test]
    fn storage_ladder_matches_daemon_except_for_stable_windows_cache_class_storage() {
        let current_dir = Path::new("/work");
        let fallback_home = Path::new("/system-home");
        let module_suffix = Path::new("cortexkit").join("aft");
        let mut env = HashMap::from([
            ("AFT_STORAGE_DIR", OsString::new()),
            ("AFT_CACHE_DIR", OsString::new()),
            ("XDG_DATA_HOME", OsString::new()),
            ("APPDATA", OsString::from("/wrong-roaming-data")),
            ("USERPROFILE", OsString::new()),
            ("HOME", OsString::new()),
            ("LOCALAPPDATA", OsString::new()),
        ]);

        for platform in [
            super::StoragePlatform::Other,
            super::StoragePlatform::Windows,
        ] {
            assert_eq!(
                resolve_storage_fixture(
                    &env,
                    Some(Path::new("")),
                    platform,
                    Some(fallback_home),
                    Some(current_dir),
                ),
                current_dir.join(".local/share").join(&module_suffix),
                "empty values and an empty configured root are unset"
            );
        }
        assert_eq!(
            resolve_storage_fixture(&env, None, super::StoragePlatform::Other, None, None,),
            PathBuf::from(".local/share/cortexkit/aft"),
            "an unavailable cwd preserves the honest relative path"
        );

        env.insert("HOME", OsString::from("/home/operator"));
        env.insert("USERPROFILE", OsString::from("/wrong-profile"));
        assert_eq!(
            resolve_storage_fixture(
                &env,
                None,
                super::StoragePlatform::Other,
                Some(fallback_home),
                Some(current_dir),
            ),
            Path::new("/home/operator/.local/share").join(&module_suffix)
        );

        env.insert("LOCALAPPDATA", OsString::from("/local-data"));
        assert_eq!(
            resolve_storage_fixture(
                &env,
                None,
                super::StoragePlatform::Windows,
                Some(fallback_home),
                Some(current_dir),
            ),
            Path::new("/local-data").join(&module_suffix)
        );
        env.insert("LOCALAPPDATA", OsString::new());
        assert_eq!(
            resolve_storage_fixture(
                &env,
                None,
                super::StoragePlatform::Windows,
                Some(fallback_home),
                Some(current_dir),
            ),
            Path::new("/wrong-profile/AppData/Local").join(&module_suffix)
        );

        env.insert("XDG_DATA_HOME", OsString::from("relative-data"));
        assert_eq!(
            resolve_storage_fixture(
                &env,
                None,
                super::StoragePlatform::Other,
                Some(fallback_home),
                Some(current_dir),
            ),
            current_dir.join("relative-data").join(&module_suffix)
        );

        env.insert("AFT_CACHE_DIR", OsString::from("/legacy-cache"));
        assert_eq!(
            resolve_storage_fixture(
                &env,
                None,
                super::StoragePlatform::Other,
                Some(fallback_home),
                Some(current_dir),
            ),
            PathBuf::from("/legacy-cache/aft")
        );
        let configured = Path::new("configured/../configured-aft");
        assert_eq!(
            resolve_storage_fixture(
                &env,
                Some(configured),
                super::StoragePlatform::Other,
                Some(fallback_home),
                Some(current_dir),
            ),
            configured,
            "the caller-owned configured spelling outranks the legacy cache lever"
        );

        env.insert("AFT_STORAGE_DIR", OsString::from("~/operator-aft"));
        assert_eq!(
            resolve_storage_fixture(
                &env,
                Some(configured),
                super::StoragePlatform::Other,
                Some(fallback_home),
                Some(current_dir),
            ),
            PathBuf::from("/home/operator/operator-aft")
        );
    }

    #[test]
    fn powershell_absence_has_an_honest_install_remedy() {
        let error = super::resolve_powershell_path_with(|_| None).expect_err("pwsh is absent");
        assert!(error.contains("PowerShell (pwsh) is not installed"));
        assert!(error.contains("https://aka.ms/powershell"));
    }

    #[test]
    fn cleanup_panic_during_unwind_does_not_abort_libtest() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let cleanup_ran = AtomicBool::new(false);
        let unwind = catch_unwind(AssertUnwindSafe(|| {
            let _cleanup = NonPanickingCleanup::new(|| {
                cleanup_ran.store(true, Ordering::SeqCst);
                panic!("forced cleanup failure");
            });
            panic!("primary test failure");
        }));

        assert!(cleanup_ran.load(Ordering::SeqCst));
        assert_eq!(
            unwind
                .expect_err("primary panic must escape the inner scope")
                .downcast_ref::<&str>(),
            Some(&"primary test failure")
        );
    }
}
