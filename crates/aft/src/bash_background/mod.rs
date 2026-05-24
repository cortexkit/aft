//! Background bash task management. Phase 0 stub; Phase 1 Track D fills in.

pub mod buffer;
pub mod persistence;
pub mod process;
pub mod registry;
pub mod watchdog;

use crate::context::AppContext;
use crate::protocol::Response;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

pub use registry::{BgCompletion, BgTaskRegistry};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BgTaskInfo {
    pub task_id: String,
    pub status: BgTaskStatus,
    pub command: String,
    pub started_at: u64,
    pub duration_ms: Option<u64>,
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
}

impl BgTaskStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            BgTaskStatus::Completed
                | BgTaskStatus::Failed
                | BgTaskStatus::Killed
                | BgTaskStatus::TimedOut
        )
    }
}

/// Spawn a bash command in the background. Returns a task_id immediately.
#[allow(clippy::too_many_arguments)]
pub fn spawn(
    request_id: &str,
    session_id: &str,
    command: &str,
    workdir: Option<PathBuf>,
    env: Option<HashMap<String, String>>,
    timeout_ms: Option<u64>,
    ctx: &AppContext,
    require_background_flag: bool,
    notify_on_completion: bool,
    compressed: bool,
) -> Response {
    if require_background_flag && !ctx.config().experimental_bash_background {
        return Response::error(
            request_id,
            "feature_disabled",
            "background bash is disabled; set `experimental.bash.background: true` in aft.jsonc",
        );
    }

    let workdir = workdir.unwrap_or_else(|| {
        ctx.config().project_root.clone().unwrap_or_else(|| {
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
        })
    });
    let storage_dir = storage_dir(ctx.config().storage_dir.as_deref());
    let max_running = ctx.config().max_background_bash_tasks;
    let timeout = timeout_ms.map(Duration::from_millis);
    let project_root = ctx
        .config()
        .project_root
        .clone()
        .or_else(|| std::env::current_dir().ok())
        .and_then(|path| std::fs::canonicalize(&path).ok().or(Some(path)));

    match ctx.bash_background().spawn(
        command,
        session_id.to_string(),
        workdir,
        env.unwrap_or_default(),
        timeout,
        storage_dir,
        max_running,
        notify_on_completion,
        compressed,
        project_root,
    ) {
        Ok(task_id) => Response::success(
            request_id,
            json!({
                "task_id": task_id,
                "status": BgTaskStatus::Running,
            }),
        ),
        Err(message) if message.contains("limit exceeded") => {
            Response::error(request_id, "background_task_limit_exceeded", message)
        }
        Err(message) => Response::error(request_id, "execution_failed", message),
    }
}

pub fn storage_dir(configured: Option<&std::path::Path>) -> PathBuf {
    if let Some(dir) = configured {
        return dir.to_path_buf();
    }
    if let Some(dir) = std::env::var_os("AFT_CACHE_DIR") {
        return PathBuf::from(dir).join("aft");
    }
    // Fallback to the user's home directory. On Unix this is `$HOME`; on
    // Windows `HOME` is typically unset, so fall back to `USERPROFILE`
    // (which is always set in interactive sessions and in the env that
    // OpenCode/Pi pass through to plugin processes). If both are missing
    // (rare — embedded contexts, broken shells), fall back to a temp
    // directory rather than `"."` — a relative path makes bg-bash wrapper
    // commands like `move /Y .\.cache\aft\... ...` fail with "system
    // cannot find the path specified" once the working directory shifts.
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    home.join(".cache").join("aft")
}
