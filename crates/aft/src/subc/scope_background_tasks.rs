//! Owner-only, bounded observation of unfinished commands. Reads do not queue
//! behind tool execution, so core can promptly check whether a worker is done.

use super::*;
use serde::{Deserialize, Serialize};

pub(super) const CAPABILITY: &str = "scope-background-tasks/v1";
pub(super) const OPERATION: &str = "scope.background_tasks";
const WALK_LIMIT: usize = 4096;
const COMMAND_BYTES: usize = 2048;

#[derive(Default)]
pub(super) struct Metrics {
    answered: std::sync::atomic::AtomicU64,
    with_running: std::sync::atomic::AtomicU64,
    invalid_params: std::sync::atomic::AtomicU64,
    owner_mismatch: std::sync::atomic::AtomicU64,
    errors: std::sync::atomic::AtomicU64,
}

impl Metrics {
    pub(super) fn snapshot(&self) -> Value {
        use std::sync::atomic::Ordering::Relaxed;
        json!({
            "answered": self.answered.load(Relaxed),
            "with_running": self.with_running.load(Relaxed),
            "refused": {
                "invalid_params": self.invalid_params.load(Relaxed),
                "scope_owner_mismatch": self.owner_mismatch.load(Relaxed),
            },
            "errors": self.errors.load(Relaxed),
        })
    }
}

struct Observation {
    success: bool,
    complete: bool,
    count: usize,
    code: Option<String>,
}

impl Observation {
    fn from_response(response: &Response) -> Self {
        Self {
            success: response.success,
            complete: response.data["complete"].as_bool().unwrap_or(false),
            count: response.data["running"].as_array().map_or(0, Vec::len),
            code: response.data["code"].as_str().map(str::to_owned),
        }
    }

    fn record(
        &self,
        metrics: &Metrics,
        principal: Option<&Principal>,
        params: &Value,
        queued: bool,
        log: &dyn Fn(&str),
    ) {
        use std::sync::atomic::Ordering::Relaxed;
        let outcome = if !queued {
            metrics.errors.fetch_add(1, Relaxed);
            "errored".to_owned()
        } else if self.success {
            metrics.answered.fetch_add(1, Relaxed);
            if self.count > 0 {
                metrics.with_running.fetch_add(1, Relaxed);
            }
            if self.complete {
                "answered".to_owned()
            } else {
                metrics.errors.fetch_add(1, Relaxed);
                "errored".to_owned()
            }
        } else {
            match self.code.as_deref() {
                Some("invalid_params") => &metrics.invalid_params,
                Some("scope_owner_mismatch") => &metrics.owner_mismatch,
                _ => &metrics.errors,
            }
            .fetch_add(1, Relaxed);
            format!("refused code={:?}", self.code)
        };
        let scope = serde_json::from_value::<Params>(params.clone())
            .ok()
            .map(|p| p.scope);
        log(&format!(
            "scope.background_tasks: caller={principal:?} owner={:?} ref={:?} epoch={:?} reported={} running={} outcome={outcome}",
            scope.as_ref().map(|s| &s.owner),
            scope.as_ref().map(|s| &s.scope_ref),
            scope.as_ref().map(|s| s.epoch),
            self.count,
            self.count,
        ));
    }
}

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

fn authorize(params: &Value, principal: Option<&Principal>) -> Result<(), Response> {
    let params = serde_json::from_value::<Params>(params.clone()).map_err(|error| {
        Response::error(
            "management-scope-background-tasks",
            "invalid_params",
            error.to_string(),
        )
    })?;
    if principal != Some(&params.scope.owner) {
        return Err(Response::error(
            "management-scope-background-tasks",
            "scope_owner_mismatch",
            "scope.background_tasks requires the scope's owner",
        ));
    }
    Ok(())
}

pub(super) fn expected_roots(
    params: &Value,
    routes: &HashMap<RouteChannel, RouteIdentity>,
) -> Result<Vec<ProjectRootId>, String> {
    let params: Params =
        serde_json::from_value(params.clone()).map_err(|error| error.to_string())?;
    let mut roots = HashSet::new();
    for (index, route) in routes.values().take(WALK_LIMIT + 1).enumerate() {
        if index == WALK_LIMIT {
            return Err("walk_limit_reached".into());
        }
        if route
            .scope
            .as_ref()
            .is_some_and(|stamp| params.scope.matches(stamp))
        {
            roots.insert(route.root.clone());
        }
    }
    Ok(roots.into_iter().collect())
}

fn registry_snapshot(
    expected: Result<Vec<ProjectRootId>, String>,
    registries: &HashMap<ProjectRootId, crate::bash_background::BgTaskRegistry>,
) -> Result<Vec<crate::bash_background::BgTaskRegistry>, String> {
    if expected?.iter().any(|root| !registries.contains_key(root)) {
        return Err("registry_not_loaded".into());
    }
    if registries.len() > WALK_LIMIT {
        return Err("walk_limit_reached".into());
    }
    Ok(registries.values().cloned().collect())
}

fn scan_saved(
    storage: &Path,
    scope: &Selector,
    known: &HashSet<String>,
    remaining: &mut usize,
    deadline: Instant,
) -> Result<(), String> {
    use crate::bash_background::persistence::{read_task_at, resolve_task_layout};
    let database = storage.join("aft.db");
    if database
        .try_exists()
        .map_err(|error| format!("store_read_failed: {error}"))?
    {
        let conn = rusqlite::Connection::open_with_flags(
            &database,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .map_err(|error| format!("store_read_failed: {error}"))?;
        conn.busy_timeout(Duration::ZERO)
            .map_err(|error| error.to_string())?;
        let mut statement = conn
            .prepare("SELECT task_id, metadata FROM bash_tasks WHERE completed_at IS NULL LIMIT ?1")
            .map_err(|error| format!("store_read_failed: {error}"))?;
        let mut rows = statement
            .query([(*remaining + 1) as i64])
            .map_err(|error| error.to_string())?;
        while let Some(row) = rows
            .next()
            .map_err(|error| format!("store_read_failed: {error}"))?
        {
            if *remaining == 0 {
                return Err("walk_limit_reached".into());
            }
            if Instant::now() >= deadline {
                return Err("time_budget_exceeded".into());
            }
            *remaining -= 1;
            let task_id: String = row.get(0).map_err(|error| error.to_string())?;
            if known.contains(&task_id) {
                continue;
            }
            let json: String = row.get(1).map_err(|error| error.to_string())?;
            let metadata: crate::bash_background::persistence::PersistedTask =
                serde_json::from_str(&json)
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
    expected: Result<Vec<ProjectRootId>, String>,
) {
    let tx = tx.clone();
    let frame = frame.clone();
    let metrics = metrics.clone();
    tokio::spawn(async move {
        let _response_task = ResponseTaskGuard::new(&metrics);
        let result = if let Err(response) = authorize(&params, principal.as_ref()) {
            response
        } else {
            let registries = metrics
                .completion_delivery
                .registries
                .try_lock()
                .map_err(|_| "registry_busy".into())
                .and_then(|registries| registry_snapshot(expected, &registries));
            if let Ok(registries) = registries {
                let answer_params = params.clone();
                let answer_principal = principal.clone();
                tokio::time::timeout(
                    Duration::from_millis(1500),
                    tokio::task::spawn_blocking(move || {
                        answer(
                            answer_params,
                            answer_principal.as_ref(),
                            &registries,
                            &storage,
                        )
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
                    serde_json::to_value(Reply::incomplete(registries.err().unwrap())).unwrap(),
                )
            }
        };
        let observation = Observation::from_response(&result);
        let sent = send_management_response(&tx, &frame, OPERATION, result, &metrics).await;
        observation.record(
            &metrics.scope_background_tasks,
            principal.as_ref(),
            &params,
            sent.is_ok(),
            &|line| log::info!("{line}"),
        );
        if let Err(error) = sent {
            log::warn!("scope.background_tasks: could not queue reply: {error}");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    fn selector() -> Selector {
        Selector {
            owner: Principal::Reserved {
                module_id: "prefrontal-core".into(),
            },
            scope_ref: "worker-7".into(),
            epoch: 3,
        }
    }

    fn observe_for_test(
        metrics: &Metrics,
        response: &Response,
        principal: Option<&Principal>,
        params: &Value,
        queued: bool,
    ) -> Vec<String> {
        let lines = std::cell::RefCell::new(Vec::new());
        Observation::from_response(response).record(metrics, principal, params, queued, &|line| {
            lines.borrow_mut().push(line.to_owned());
        });
        lines.into_inner()
    }

    #[test]
    fn scope_background_tasks_empty_and_refused_observations() {
        let dir = tempfile::tempdir().unwrap();
        let scope = selector();
        let params = json!({"scope": scope});
        let metrics = Metrics::default();
        let response = answer(params.clone(), Some(&scope.owner), &[], dir.path());
        let lines = observe_for_test(&metrics, &response, Some(&scope.owner), &params, true);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("reported=0 running=0 outcome=answered"));
        assert!(lines[0].contains("worker-7"));
        assert!(lines[0].contains("prefrontal-core"));
        assert!(lines[0].contains("epoch=Some(3)"));
        assert_eq!(metrics.snapshot()["answered"], 1);
        assert_eq!(metrics.snapshot()["with_running"], 0);

        let caller = Principal::Direct {};
        let response = authorize(&params, Some(&caller)).unwrap_err();
        let lines = observe_for_test(&metrics, &response, Some(&caller), &params, true);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("outcome=refused code=Some(\"scope_owner_mismatch\")"));
        assert_eq!(metrics.snapshot()["refused"]["scope_owner_mismatch"], 1);
        assert_eq!(metrics.snapshot()["answered"], 1);
    }

    #[test]
    fn scope_background_tasks_errors_are_counted() {
        let scope = selector();
        let params = json!({"scope": scope});
        let metrics = Metrics::default();
        let response = Response::success(
            "test",
            serde_json::to_value(Reply::incomplete("registry_busy")).unwrap(),
        );
        let lines = observe_for_test(&metrics, &response, Some(&scope.owner), &params, true);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("outcome=errored"));
        assert_eq!(metrics.snapshot()["answered"], 1);
        assert_eq!(metrics.snapshot()["errors"], 1);
        let lines = observe_for_test(&metrics, &response, Some(&scope.owner), &params, false);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("outcome=errored"));
        assert_eq!(metrics.snapshot()["answered"], 1);
        assert_eq!(metrics.snapshot()["errors"], 2);
    }

    #[test]
    fn scope_background_tasks_serializer_vectors_do_not_drift() {
        let dir = Path::new(crate::test_support::manifest_dir())
            .join("../../test-vectors/scope-background-tasks-v1");
        let params = Params { scope: selector() };
        let request = json!({"op": OPERATION, "params": params});
        let mut sums = String::new();
        let cases = [
            (
                "running",
                Response::success(
                    "management-scope-background-tasks",
                    serde_json::to_value(Reply {
                        running: vec![Running {
                            task_id: "bash-0000000000000001".into(),
                            title: "cargo test".into(),
                            command: "cargo test".into(),
                            started_at_ms: 1730000000000,
                            remote: Some(Remote {
                                job_id: "00000000-0000-4000-8000-000000000007".into(),
                            }),
                            command_truncated: false,
                        }],
                        complete: true,
                        reason: None,
                    })
                    .unwrap(),
                ),
            ),
            (
                "empty",
                Response::success(
                    "management-scope-background-tasks",
                    serde_json::to_value(Reply {
                        running: vec![],
                        complete: true,
                        reason: None,
                    })
                    .unwrap(),
                ),
            ),
            (
                "incomplete",
                Response::success(
                    "management-scope-background-tasks",
                    serde_json::to_value(Reply::incomplete("registry_not_loaded")).unwrap(),
                ),
            ),
            (
                "refused",
                authorize(&request["params"], Some(&Principal::Direct {})).unwrap_err(),
            ),
        ];
        let update = std::env::var_os("UPDATE_AFT_SCOPE_VECTORS").is_some();
        if update {
            std::fs::create_dir_all(&dir).unwrap();
        }
        for (name, response) in cases {
            for (suffix, value) in [
                ("request", request.clone()),
                ("response", management_response_body(OPERATION, response)),
            ] {
                let file = format!("{name}.{suffix}.json");
                let bytes = format!("{}\n", serde_json::to_string_pretty(&value).unwrap());
                if update {
                    std::fs::write(dir.join(&file), &bytes).unwrap();
                }
                assert_eq!(
                    std::fs::read_to_string(dir.join(&file)).unwrap(),
                    bytes,
                    "serializer drift in {file}"
                );
                sums.push_str(&format!("{:x}  {file}\n", Sha256::digest(bytes.as_bytes())));
            }
        }
        if update {
            std::fs::write(dir.join("SHA256SUMS"), &sums).unwrap();
        }
        assert_eq!(
            std::fs::read_to_string(dir.join("SHA256SUMS")).unwrap(),
            sums
        );
    }

    #[test]
    fn scope_background_tasks_refuses_non_owner_before_reading_store() {
        let params = serde_json::to_value(Params { scope: selector() }).unwrap();
        for principal in [
            None,
            Some(Principal::Direct {}),
            Some(Principal::Reserved {
                module_id: "broca".into(),
            }),
        ] {
            let reply = answer(
                params.clone(),
                principal.as_ref(),
                &[],
                Path::new("/not-a-task-store"),
            );
            assert!(!reply.success);
            assert_eq!(reply.data["code"], "scope_owner_mismatch");
        }
    }

    #[test]
    fn scope_background_tasks_masks_and_caps_commands_on_utf8_boundaries() {
        let mut metadata = crate::bash_background::persistence::PersistedTask::starting(
            "bash-test".into(),
            "s".into(),
            format!("TOKEN=secret {}", "é".repeat(3000)),
            PathBuf::from("."),
            None,
            None,
            true,
            false,
        );
        metadata.command = format!(
            "curl -H 'Authorization: Bearer ghp_{}' {}",
            "X".repeat(36),
            "é".repeat(3000)
        );
        let row = running(&metadata);
        assert!(row.command_truncated);
        assert!(row.command.len() <= COMMAND_BYTES);
        assert!(!row.command.contains(&"X".repeat(36)));
        assert_eq!(row.command, row.title);
    }

    #[test]
    fn scope_background_tasks_store_errors_are_incomplete() {
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path().join("not-a-directory");
        std::fs::write(&storage, "x").unwrap();
        let scope = selector();
        let reply = answer(json!({"scope":scope}), Some(&scope.owner), &[], &storage);
        assert!(reply.success);
        assert_eq!(reply.data["complete"], false);
        assert!(reply.data["reason"]
            .as_str()
            .unwrap()
            .starts_with("store_read_failed"));
    }

    #[test]
    fn scope_background_tasks_unloaded_scope_registry_is_not_empty_complete() {
        let dir = tempfile::tempdir().unwrap();
        let selected = selector();
        let stamp = subc_protocol::scope::ScopeStamp::new(
            selected.owner.clone(),
            selected.scope_ref.clone(),
            selected.epoch,
            subc_protocol::scope::ScopeKind::Worker,
        );
        let task = crate::bash_background::persistence::create_task_layout(
            dir.path(),
            "session",
            "bash-0000000000000001",
        )
        .unwrap();
        let mut metadata = crate::bash_background::persistence::PersistedTask::starting(
            "bash-0000000000000001".into(),
            "session".into(),
            "sleep 30".into(),
            dir.path().into(),
            None,
            None,
            true,
            false,
        );
        metadata.scope = Some(stamp);
        crate::bash_background::persistence::write_task_at(&task, &metadata).unwrap();
        let reply = answer(
            json!({"scope": selected}),
            Some(&selected.owner),
            &[],
            dir.path(),
        );
        assert_eq!(reply.data["complete"], false);
        assert_eq!(reply.data["reason"], "registry_not_loaded");
        let wrong_epoch = Selector {
            epoch: 4,
            ..selector()
        };
        let reply = answer(
            json!({"scope": wrong_epoch}),
            Some(&wrong_epoch.owner),
            &[],
            dir.path(),
        );
        assert_eq!(reply.data["complete"], true);
        assert_eq!(reply.data["running"], json!([]));
    }

    #[test]
    fn scope_background_tasks_bound_root_without_registry_is_incomplete_even_without_saved_tasks() {
        let (_dir, root) = super::super::test_support::test_root("scope-root-not-ready");
        let selected = selector();
        let mut route = super::super::test_support::route_identity_with_harness(
            &root,
            "session",
            BindTrust::FirstParty,
            "runner",
        );
        Arc::get_mut(&mut route.0).unwrap().scope = Some(subc_protocol::scope::ScopeStamp::new(
            selected.owner.clone(),
            selected.scope_ref.clone(),
            selected.epoch,
            subc_protocol::scope::ScopeKind::Worker,
        ));
        let expected = expected_roots(
            &json!({"scope":selected}),
            &HashMap::from([(route_key(1, 1), route)]),
        );
        assert_eq!(expected.as_ref().unwrap(), &vec![root]);
        assert!(
            matches!(registry_snapshot(expected, &HashMap::new()), Err(reason) if reason == "registry_not_loaded")
        );
    }

    #[test]
    fn scope_background_tasks_remote_rows_include_job_identity() {
        let mut metadata = crate::bash_background::persistence::PersistedTask::starting(
            "bash-remote".into(),
            "session".into(),
            "cargo test".into(),
            PathBuf::from("."),
            None,
            None,
            true,
            false,
        );
        metadata.remote = Some(serde_json::from_value(json!({"harness":"runner", "session":"session", "stdout_len":0, "stderr_len":0, "cancel_requested":false, "job_id":"00000000-0000-4000-8000-000000000007"})).unwrap());
        assert_eq!(
            serde_json::to_value(running(&metadata)).unwrap()["remote"]["job_id"],
            "00000000-0000-4000-8000-000000000007"
        );
    }

    #[cfg(unix)]
    #[test]
    fn scope_background_tasks_lists_live_tasks_without_waiting_for_busy_state() {
        let dir = tempfile::tempdir().unwrap();
        let registry = crate::bash_background::BgTaskRegistry::default();
        let selected = selector();
        let stamp = subc_protocol::scope::ScopeStamp::new(
            selected.owner.clone(),
            selected.scope_ref.clone(),
            selected.epoch,
            subc_protocol::scope::ScopeKind::Worker,
        );
        let id = crate::bash_background::registry::with_scope_spawn(Some(stamp), || {
            registry.spawn_with_shell_in_slot(
                crate::sandbox_spawn::SpawnPlan::Unsandboxed,
                "sleep 30",
                crate::bash_background::BashShell::Bash,
                crate::bash_background::registry::resolve_posix_shell(),
                "session".into(),
                dir.path().into(),
                HashMap::new(),
                crate::bash_background::HardKill::After(Duration::from_secs(30)),
                None,
                dir.path().into(),
                crate::bash_background::TaskSlot::Background { max: 10 },
                true,
                false,
                Some(dir.path().into()),
            )
        })
        .unwrap();
        let reply = answer(
            json!({"scope":selected}),
            Some(&selected.owner),
            &[registry.clone()],
            dir.path(),
        );
        assert_eq!(reply.data["complete"], true, "{reply:?}");
        assert_eq!(reply.data["running"][0]["task_id"], id);
        assert_eq!(reply.data["running"][0]["command"], "sleep 30");
        let metrics = Metrics::default();
        let params = json!({"scope": selected});
        let lines = observe_for_test(&metrics, &reply, Some(&selected.owner), &params, true);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("reported=1 running=1 outcome=answered"));
        assert!(!lines[0].contains("sleep 30"));
        assert_eq!(metrics.snapshot()["answered"], 1);
        assert_eq!(metrics.snapshot()["with_running"], 1);
        let task = registry.task_for_test(&id).unwrap();
        let locked = task.state.lock().unwrap();
        let start = Instant::now();
        let reply = answer(
            json!({"scope":selected}),
            Some(&selected.owner),
            &[registry.clone()],
            dir.path(),
        );
        assert!(start.elapsed() < Duration::from_secs(1));
        assert_eq!(reply.data["complete"], false);
        assert_eq!(reply.data["reason"], "task_busy");
        drop(locked);
        registry.kill(&id, "session").unwrap();
        registry.shutdown();
    }
}
