//! Owner-only, bounded observation of unfinished commands. Reads do not queue
//! behind tool execution, so core can promptly check whether a worker is done.

use super::*;
use serde::{Deserialize, Serialize};

pub(super) const CAPABILITY: &str = "scope-background-tasks/v1";
pub(super) const OPERATION: &str = "scope.background_tasks";
const WALK_LIMIT: usize = 4096;
const COMMAND_BYTES: usize = 2048;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Selector {
    pub owner: Principal,
    #[serde(rename = "ref")]
    pub scope_ref: String,
    pub epoch: u64,
}

impl Selector {
    pub(crate) fn matches(&self, stamp: &subc_protocol::scope::ScopeStamp) -> bool {
        self.owner == stamp.owner
            && self.scope_ref == stamp.scope_ref
            && self.epoch == stamp.scope_epoch
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Params {
    scope: Selector,
}

#[derive(Debug, Serialize)]
pub(crate) struct Running {
    pub task_id: String,
    pub title: String,
    pub command: String,
    pub started_at_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote: Option<Remote>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub command_truncated: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct Remote {
    pub job_id: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct Reply {
    pub running: Vec<Running>,
    pub complete: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl Reply {
    fn incomplete(reason: impl Into<String>) -> Self {
        Self {
            running: Vec::new(),
            complete: false,
            reason: Some(reason.into()),
        }
    }
}

pub(crate) fn running(metadata: &crate::bash_background::persistence::PersistedTask) -> Running {
    let mut command = crate::log_redact::redact_command(&metadata.command).into_owned();
    let command_truncated = command.len() > COMMAND_BYTES;
    if command_truncated {
        let mut end = COMMAND_BYTES;
        while !command.is_char_boundary(end) {
            end -= 1;
        }
        command.truncate(end);
    }
    Running {
        task_id: metadata.task_id.clone(),
        title: command.clone(),
        command,
        command_truncated,
        started_at_ms: metadata.started_at,
        remote: metadata
            .remote
            .as_ref()
            .and_then(|remote| remote.job_id)
            .map(|job_id| Remote {
                job_id: job_id.to_string(),
            }),
    }
}

fn answer(
    params: Value,
    principal: Option<&Principal>,
    registries: &[crate::bash_background::BgTaskRegistry],
    storage: &Path,
) -> Response {
    const ID: &str = "management-scope-background-tasks";
    let params = match serde_json::from_value::<Params>(params) {
        Ok(params) => params,
        Err(error) => return Response::error(ID, "invalid_params", error.to_string()),
    };
    if principal != Some(&params.scope.owner) {
        return Response::error(
            ID,
            "scope_owner_mismatch",
            "scope.background_tasks requires the scope's owner",
        );
    }
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut reply = Reply {
        running: Vec::new(),
        complete: true,
        reason: None,
    };
    let mut known = HashSet::new();
    let mut remaining = WALK_LIMIT;
    for registry in registries {
        match registry.try_scope_background_tasks(&params.scope, &mut remaining) {
            Ok((ids, running)) => {
                known.extend(ids);
                reply.running.extend(running);
            }
            Err(reason) => {
                reply.complete = false;
                reply.reason.get_or_insert(reason);
            }
        }
    }
    // A project not opened since AFT restarted may have tasks only on disk.
    // Report that uncertainty without restoring them: a read must not start a
    // remote job worker or probe a process.
    let scan = scan_saved(storage, &params.scope, &known, &mut remaining, deadline);
    if let Err(reason) = scan {
        reply.complete = false;
        reply.reason.get_or_insert(reason);
    }
    reply.running.sort_by(|a, b| a.task_id.cmp(&b.task_id));
    reply.running.dedup_by(|a, b| a.task_id == b.task_id);
    Response::success(
        ID,
        serde_json::to_value(reply).expect("scope task reply is JSON"),
    )
}

fn scan_saved(
    storage: &Path,
    scope: &Selector,
    known: &HashSet<String>,
    remaining: &mut usize,
    deadline: Instant,
) -> Result<(), String> {
    use crate::bash_background::persistence::{read_task_at, resolve_task_layout};
    fn entries(path: &Path) -> Result<Option<std::fs::ReadDir>, String> {
        match std::fs::read_dir(path) {
            Ok(entries) => Ok(Some(entries)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(format!("store_read_failed: {error}")),
        }
    }
    fn next_entry(
        entries: &mut std::fs::ReadDir,
        remaining: &mut usize,
        deadline: Instant,
    ) -> Result<Option<std::fs::DirEntry>, String> {
        if Instant::now() >= deadline {
            return Err("time_budget_exceeded".into());
        }
        if *remaining == 0 {
            return Err("walk_limit_reached".into());
        }
        *remaining -= 1;
        entries
            .next()
            .transpose()
            .map_err(|error| format!("store_read_failed: {error}"))
    }
    let mut dirs = vec![storage.join("bash-tasks")];
    if let Some(mut roots) = entries(storage)? {
        while let Some(root) = next_entry(&mut roots, remaining, deadline)? {
            if root
                .file_type()
                .map_err(|error| error.to_string())?
                .is_dir()
            {
                dirs.push(root.path().join("bash-tasks"));
            }
        }
    }
    for dir in dirs {
        let Some(mut sessions) = entries(&dir)? else {
            continue;
        };
        while let Some(session) = next_entry(&mut sessions, remaining, deadline)? {
            if !session
                .file_type()
                .map_err(|error| error.to_string())?
                .is_dir()
            {
                continue;
            }
            let Some(mut tasks) = entries(&session.path())? else {
                return Err("store_read_failed: session disappeared".into());
            };
            let mut seen = HashSet::new();
            while let Some(task) = next_entry(&mut tasks, remaining, deadline)? {
                let name = task.file_name();
                let Some(name) = name.to_str() else {
                    return Err("store_read_failed: non-UTF8 task".into());
                };
                let id = name.split('.').next().unwrap_or(name);
                if !id.starts_with("bash-") || !seen.insert(id.to_string()) || known.contains(id) {
                    continue;
                }
                let metadata = resolve_task_layout(&session.path(), id)
                    .and_then(|task| read_task_at(&task))
                    .map_err(|error| format!("store_read_failed: {error}"))?;
                if metadata
                    .scope
                    .as_ref()
                    .is_some_and(|stamp| scope.matches(stamp))
                    && !metadata.is_terminal()
                {
                    return Err("registry_not_loaded".into());
                }
            }
        }
    }
    Ok(())
}

pub(super) fn spawn(
    tx: &WriterSender,
    frame: &Frame,
    metrics: &Arc<DispatchPathMetrics>,
    principal: Option<Principal>,
    params: Value,
    storage: PathBuf,
) {
    let tx = tx.clone();
    let frame = frame.clone();
    let metrics = metrics.clone();
    tokio::spawn(async move {
        let _response_task = ResponseTaskGuard::new(&metrics);
        let registries = metrics
            .completion_delivery
            .registries
            .try_lock()
            .ok()
            .map(|registries| registries.values().cloned().collect::<Vec<_>>());
        let result = if let Some(registries) = registries {
            tokio::time::timeout(
                Duration::from_millis(1500),
                tokio::task::spawn_blocking(move || {
                    answer(params, principal.as_ref(), &registries, &storage)
                }),
            )
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_else(|| {
                Response::success(
                    "management-scope-background-tasks",
                    serde_json::to_value(Reply::incomplete("time_budget_exceeded")).unwrap(),
                )
            })
        } else {
            Response::success(
                "management-scope-background-tasks",
                serde_json::to_value(Reply::incomplete("registry_busy")).unwrap(),
            )
        };
        if let Err(error) = send_management_response(&tx, &frame, OPERATION, result, &metrics).await
        {
            log::warn!("scope.background_tasks: could not queue reply: {error}");
        }
    });
}
