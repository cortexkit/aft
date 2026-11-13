//! Owner-only, bounded observation of unfinished commands. Reads do not queue
//! behind tool execution, so core can promptly check whether a worker is done.

use super::*;
use serde::{Deserialize, Serialize};

pub(super) const CAPABILITY: &str = "scope-background-tasks/v1";
pub(super) const OPERATION: &str = "scope.background_tasks";
const WALK_LIMIT: usize = 4096;
const COMMAND_BYTES: usize = 2048;
const INCOMPLETE_REASONS: [&str; 7] = [
    "registry_busy",
    "task_busy",
    "registry_not_loaded",
    "walk_limit_reached",
    "time_budget_exceeded",
    "store_read_failed",
    "other",
];

#[derive(Default)]
pub(super) struct Metrics {
    answered: std::sync::atomic::AtomicU64,
    with_running: std::sync::atomic::AtomicU64,
    invalid_params: std::sync::atomic::AtomicU64,
    owner_mismatch: std::sync::atomic::AtomicU64,
    errors: std::sync::atomic::AtomicU64,
    incomplete: [std::sync::atomic::AtomicU64; INCOMPLETE_REASONS.len()],
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
            "incomplete": INCOMPLETE_REASONS.iter().zip(&self.incomplete)
                .map(|(reason, count)| ((*reason).to_owned(), json!(count.load(Relaxed))))
                .collect::<serde_json::Map<_, _>>(),
            "store_writes": crate::bash_background::store_writes::snapshot(),
        })
    }
}

struct Observation {
    success: bool,
    complete: bool,
    count: usize,
    code: Option<String>,
    reason: Option<String>,
}

impl Observation {
    fn from_response(response: &Response) -> Self {
        Self {
            success: response.success,
            complete: response.data["complete"].as_bool().unwrap_or(false),
            count: response.data["running"].as_array().map_or(0, Vec::len),
            code: response.data["code"].as_str().map(str::to_owned),
            reason: response.data["reason"].as_str().map(str::to_owned),
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
        // Only fixed reason codes enter telemetry; store error details and
        // task commands are never included in this per-call log.
        let reason = self
            .reason
            .as_deref()
            .unwrap_or("other")
            .split(':')
            .next()
            .unwrap();
        let reason_index = INCOMPLETE_REASONS
            .iter()
            .position(|code| *code == reason)
            .unwrap_or(INCOMPLETE_REASONS.len() - 1);
        if self.success && !self.complete {
            metrics.incomplete[reason_index].fetch_add(1, Relaxed);
        }
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
        let reason_field = if self.success && !self.complete {
            format!(" reason={}", INCOMPLETE_REASONS[reason_index])
        } else {
            String::new()
        };
        log(&format!(
            "scope.background_tasks: caller={principal:?} owner={:?} ref={:?} epoch={:?} running={} complete={} outcome={outcome}{reason_field}",
            scope.as_ref().map(|s| &s.owner),
            scope.as_ref().map(|s| &s.scope_ref),
            scope.as_ref().map(|s| s.epoch),
            self.count,
            self.complete,
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
    sessions: &[PathBuf],
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
    // A project not opened since AFT restarted may have tasks only in storage.
    // Report that uncertainty without restoring them: a read must not start a
    // remote job worker or probe a process.
    let scan = scan_saved(
        storage,
        &params.scope,
        &known,
        &mut remaining,
        deadline,
        sessions,
    );
    if let Err(reason) = scan {
        reply.complete = false;
        reply.reason.get_or_insert(reason);
    }
    if !crate::bash_background::store_writes::authoritative(storage) {
        reply.complete = false;
        reply.reason = Some("store_read_failed".into());
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

#[derive(Default)]
pub(super) struct Expected {
    roots: Vec<ProjectRootId>,
    sessions: Vec<PathBuf>,
}

pub(super) fn expected_roots(
    params: &Value,
    routes: &HashMap<RouteChannel, RouteIdentity>,
    storage: &Path,
) -> Result<Expected, String> {
    let params: Params =
        serde_json::from_value(params.clone()).map_err(|error| error.to_string())?;
    let mut roots = HashSet::new();
    let mut sessions = HashSet::new();
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
            let hash = crate::backup::hash_session(&route.session);
            sessions.insert(storage.join("bash-tasks").join(&hash));
            let harness = route
                .harness
                .parse::<crate::harness::Harness>()
                .map_err(|_| "store_read_failed".to_string())?;
            sessions.insert(
                storage
                    .join(harness.storage_segment())
                    .join("bash-tasks")
                    .join(hash),
            );
        }
    }
    Ok(Expected {
        roots: roots.into_iter().collect(),
        sessions: sessions.into_iter().collect(),
    })
}

fn registry_snapshot(
    expected: Result<Expected, String>,
    registries: &HashMap<ProjectRootId, crate::bash_background::BgTaskRegistry>,
) -> Result<(Vec<crate::bash_background::BgTaskRegistry>, Vec<PathBuf>), String> {
    let expected = expected?;
    if expected
        .roots
        .iter()
        .any(|root| !registries.contains_key(root))
    {
        return Err("registry_not_loaded".into());
    }
    if registries.len() > WALK_LIMIT {
        return Err("walk_limit_reached".into());
    }
    Ok((registries.values().cloned().collect(), expected.sessions))
}

// Match the existing non-terminal partial index before inspecting scope JSON.
// Only matching rows consume the management observation's iterator budget.
const SCOPED_TASKS_SQL: &str = "SELECT task_id, metadata FROM bash_tasks
    WHERE status NOT IN ('completed', 'failed', 'killed', 'timed_out')
      AND completed_at IS NULL
      AND CASE WHEN json_valid(metadata) THEN
          json_extract(metadata, '$.scope.owner') = json(?1)
          AND json_extract(metadata, '$.scope.ref') = ?2
          AND json_extract(metadata, '$.scope.epoch') = json_extract(?3, '$')
          ELSE 0 END
    LIMIT ?4";

fn scan_saved(
    storage: &Path,
    scope: &Selector,
    known: &HashSet<String>,
    remaining: &mut usize,
    deadline: Instant,
    sessions: &[PathBuf],
) -> Result<(), String> {
    if !crate::bash_background::store_writes::authoritative(storage) {
        return Err("store_read_failed".into());
    }
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
            .prepare(SCOPED_TASKS_SQL)
            .map_err(|error| format!("store_read_failed: {error}"))?;
        let mut rows = statement
            .query(rusqlite::params![
                serde_json::to_string(&scope.owner).expect("principal is JSON"),
                scope.scope_ref,
                scope.epoch.to_string(),
                (*remaining + 1) as i64,
            ])
            .map_err(|error| format!("store_read_failed: {error}"))?;
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
        // JSON can precede its DB mirror. Live routes identify exactly the
        // sessions needing disk fallback; outstanding dual writes above make
        // the DB non-authoritative. An old task folder never imported into
        // bash_tasks, with no matching live route, remains undiscoverable here
        // until opening its project scans and backfills the missing DB row.
        // Worker scopes retain their live routes during completion checks.
        return scan_sessions(sessions, scope, known, remaining, deadline);
    }
    // With no database, retain the bounded legacy discovery path.
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
            if session
                .file_type()
                .map_err(|error| error.to_string())?
                .is_dir()
            {
                scan_sessions(&[session.path()], scope, known, remaining, deadline)?;
            }
        }
    }
    Ok(())
}

fn entries(path: &Path) -> Result<Option<std::fs::ReadDir>, String> {
    match std::fs::read_dir(path) {
        Ok(entries) => Ok(Some(entries)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("store_read_failed: {error}")),
    }
}

fn next_entry<T, E: std::fmt::Display>(
    entries: &mut impl Iterator<Item = Result<T, E>>,
    remaining: &mut usize,
    deadline: Instant,
) -> Result<Option<T>, String> {
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

fn scan_sessions(
    sessions: &[PathBuf],
    scope: &Selector,
    known: &HashSet<String>,
    remaining: &mut usize,
    deadline: Instant,
) -> Result<(), String> {
    use crate::bash_background::persistence::{read_task_at, resolve_task_layout};
    for session in sessions {
        if Instant::now() >= deadline {
            return Err("time_budget_exceeded".into());
        }
        let Some(mut tasks) = entries(session)? else {
            continue;
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
            let metadata = resolve_task_layout(session, id)
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
    Ok(())
}

pub(super) fn spawn(
    tx: &WriterSender,
    frame: &Frame,
    metrics: &Arc<DispatchPathMetrics>,
    principal: Option<Principal>,
    params: Value,
    storage: PathBuf,
    expected: Result<Expected, String>,
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
            if let Ok((registries, sessions)) = registries {
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
                            &sessions,
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

    #[tokio::test]
    async fn scope_background_tasks_spawn_records_answers_and_refusals() {
        let dir = tempfile::tempdir().unwrap();
        let selected = selector();
        let params = json!({"scope": selected});
        let metrics = Arc::new(DispatchPathMetrics::new());
        let (writer, mut replies) = tokio::sync::mpsc::channel(8);
        let frame = Frame::build(FrameType::Request, control_flags(), 41, 1, 1, vec![]).unwrap();
        spawn(
            &writer,
            &frame,
            &metrics,
            Some(selected.owner.clone()),
            params.clone(),
            dir.path().into(),
            Ok(Expected::default()),
        );
        replies.recv().await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while metrics.scope_background_tasks.snapshot()["answered"] != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        spawn(
            &writer,
            &frame,
            &metrics,
            Some(Principal::Direct {}),
            params,
            dir.path().into(),
            Ok(Expected::default()),
        );
        replies.recv().await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while metrics.scope_background_tasks.snapshot()["refused"]["scope_owner_mismatch"] != 1
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            metrics.snapshot(&HashMap::new())["scope_background_tasks"]["answered"],
            1
        );
        assert_eq!(
            metrics.snapshot(&HashMap::new())["scope_background_tasks"]["refused"]
                ["scope_owner_mismatch"],
            1
        );
    }

    #[test]
    fn scope_background_tasks_empty_and_refused_observations() {
        let dir = tempfile::tempdir().unwrap();
        let scope = selector();
        let params = json!({"scope": scope});
        let metrics = Metrics::default();
        let response = answer(params.clone(), Some(&scope.owner), &[], dir.path(), &[]);
        let lines = observe_for_test(&metrics, &response, Some(&scope.owner), &params, true);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("running=0 complete=true outcome=answered"));
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
        assert!(lines[0].contains("running=0 complete=false"));
        assert!(lines[0].contains("reason=registry_busy"));
        assert!(!lines[0].contains("reported="));
        assert_eq!(metrics.snapshot()["answered"], 1);
        assert_eq!(metrics.snapshot()["errors"], 1);
        assert_eq!(metrics.snapshot()["incomplete"]["registry_busy"], 1);
        let lines = observe_for_test(&metrics, &response, Some(&scope.owner), &params, false);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("outcome=errored"));
        assert_eq!(metrics.snapshot()["answered"], 1);
        assert_eq!(metrics.snapshot()["errors"], 2);
        assert_eq!(metrics.snapshot()["incomplete"]["registry_busy"], 2);
        for reason in &INCOMPLETE_REASONS[1..] {
            let response = Response::success(
                "test",
                serde_json::to_value(Reply::incomplete(if *reason == "store_read_failed" {
                    "store_read_failed: command-secret"
                } else {
                    reason
                }))
                .unwrap(),
            );
            let lines = observe_for_test(&metrics, &response, Some(&scope.owner), &params, true);
            assert_eq!(lines.len(), 1);
            assert!(lines[0].contains(&format!("reason={reason}")));
            assert!(!lines[0].contains("command-secret"));
            assert_eq!(metrics.snapshot()["incomplete"][*reason], 1);
        }
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
                &[],
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
        let reply = answer(
            json!({"scope":scope}),
            Some(&scope.owner),
            &[],
            &storage,
            &[],
        );
        assert!(reply.success);
        assert_eq!(reply.data["complete"], false);
        assert!(reply.data["reason"]
            .as_str()
            .unwrap()
            .starts_with("store_read_failed"));
    }

    fn large_saved_store(storage: &Path) {
        use crate::bash_background::persistence::PersistedTask;

        let conn = crate::db::open(&storage.join("aft.db")).unwrap();
        let tx = conn.unchecked_transaction().unwrap();
        let mut other = selector();
        other.scope_ref = "another-worker".into();
        for index in 0..435 {
            let mut metadata = PersistedTask::starting(
                format!("bash-{:016x}", 100_000 + index),
                "other-session".into(),
                "sleep 30".into(),
                storage.into(),
                None,
                None,
                true,
                false,
            );
            metadata.scope = Some(subc_protocol::scope::ScopeStamp::new(
                other.owner.clone(),
                other.scope_ref.clone(),
                other.epoch,
                subc_protocol::scope::ScopeKind::Worker,
            ));
            let paths = crate::bash_background::persistence::task_paths(
                storage,
                "other-session",
                &metadata.task_id,
            )
            .unwrap();
            crate::db::bash_tasks::upsert_bash_task(
                &tx,
                &metadata.to_bash_task_row("opencode", &paths).unwrap(),
            )
            .unwrap();
        }
        // Match the live store's 23,833 session/task entries in both layouts.
        for session in 0..233 {
            let root = if session % 2 == 0 {
                storage.to_path_buf()
            } else {
                storage.join("project-root")
            };
            let session_dir = root.join("bash-tasks").join(format!("session-{session}"));
            std::fs::create_dir_all(&session_dir).unwrap();
            let count = 101 + usize::from(session < 67);
            for task in 0..count {
                let id = format!("bash-{:016x}", session * 1000 + task);
                let mut metadata = PersistedTask::starting(
                    id.clone(),
                    format!("session-{session}"),
                    "true".into(),
                    storage.into(),
                    None,
                    None,
                    true,
                    false,
                );
                metadata.status = crate::bash_background::BgTaskStatus::Completed;
                metadata.finished_at = Some(1);
                let task_dir = session_dir.join(&id);
                std::fs::create_dir_all(task_dir.join("control")).unwrap();
                std::fs::create_dir(task_dir.join("io")).unwrap();
                std::fs::write(
                    task_dir.join("control/metadata.json"),
                    serde_json::to_vec(&metadata).unwrap(),
                )
                .unwrap();
                let paths = crate::bash_background::persistence::task_paths(
                    storage,
                    &metadata.session_id,
                    &id,
                )
                .unwrap();
                crate::db::bash_tasks::upsert_bash_task(
                    &tx,
                    &metadata.to_bash_task_row("opencode", &paths).unwrap(),
                )
                .unwrap();
            }
        }
        tx.execute_batch("WITH RECURSIVE history(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM history WHERE n < 136045)
            INSERT INTO bash_tasks (harness, session_id, task_id, project_key, command, cwd, status, started_at, completed_at, metadata)
            SELECT 'opencode', 'archived', printf('bash-archive-%d', n), 'archive', 'true', '.', 'completed', 0, 1, '{}' FROM history;")
            .unwrap();
        tx.commit().unwrap();
        assert_eq!(
            conn.query_row("SELECT count(*) FROM bash_tasks", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            160_080
        );
    }

    #[test]
    fn scope_background_tasks_large_saved_tree_and_unrelated_rows_are_complete() {
        let dir = tempfile::tempdir().unwrap();
        large_saved_store(dir.path());
        let selected = selector();
        let params = json!({"scope": selected});
        let conn = rusqlite::Connection::open(dir.path().join("aft.db")).unwrap();
        let plan: Vec<String> = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {SCOPED_TASKS_SQL}"))
            .unwrap()
            .query_map(
                rusqlite::params![
                    serde_json::to_string(&selected.owner).unwrap(),
                    selected.scope_ref,
                    selected.epoch.to_string(),
                    WALK_LIMIT as i64
                ],
                |row| row.get(3),
            )
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        eprintln!("scope task query plan: {plan:?}");
        assert!(
            plan.iter()
                .any(|line| line.contains("idx_bash_tasks_non_terminal_pid")),
            "{plan:?}"
        );
        let mut remaining = 1;
        scan_saved(
            dir.path(),
            &selected,
            &HashSet::new(),
            &mut remaining,
            Instant::now() + Duration::from_secs(1),
            &[],
        )
        .unwrap();
        assert_eq!(
            remaining, 1,
            "unrelated rows must not consume the scope budget"
        );
        let mut replies = Vec::new();
        for call in 1..=5 {
            let started = Instant::now();
            let reply = answer(
                params.clone(),
                Some(&selected.owner),
                &[],
                dir.path(),
                &[dir.path().join("bash-tasks/session-0")],
            );
            eprintln!(
                "large saved store call {call}: {} elapsed={:?}",
                reply.data,
                started.elapsed()
            );
            replies.push(reply);
        }
        for reply in replies {
            assert_eq!(reply.data["complete"], true, "{reply:?}");
            assert_eq!(reply.data["running"], json!([]));
        }
    }

    #[test]
    fn scope_background_tasks_walk_limit_stops_before_advancing_iterator() {
        let mut advanced = 0;
        let mut iter = std::iter::from_fn(|| {
            advanced += 1;
            Some(Ok::<_, std::io::Error>(advanced))
        });
        let mut remaining = 3;
        let deadline = Instant::now() + Duration::from_secs(1);
        for expected in 1..=3 {
            assert_eq!(
                next_entry(&mut iter, &mut remaining, deadline).unwrap(),
                Some(expected)
            );
        }
        assert_eq!(
            next_entry(&mut iter, &mut remaining, deadline),
            Err("walk_limit_reached".into())
        );
        drop(iter);
        assert_eq!(advanced, 3);
    }

    #[test]
    fn scope_background_tasks_store_only_running_task_is_not_complete() {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("aft.db")).unwrap();
        let selected = selector();
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
        metadata.scope = Some(subc_protocol::scope::ScopeStamp::new(
            selected.owner.clone(),
            selected.scope_ref.clone(),
            selected.epoch,
            subc_protocol::scope::ScopeKind::Worker,
        ));
        let paths = crate::bash_background::persistence::task_paths(
            dir.path(),
            "session",
            &metadata.task_id,
        )
        .unwrap();
        crate::db::bash_tasks::upsert_bash_task(
            &conn,
            &metadata.to_bash_task_row("opencode", &paths).unwrap(),
        )
        .unwrap();
        let reply = answer(
            json!({"scope": selected}),
            Some(&selected.owner),
            &[],
            dir.path(),
            &[],
        );
        assert_eq!(reply.data["reason"], "registry_not_loaded", "{reply:?}");
        assert_eq!(reply.data["complete"], false);
        for other in [
            Selector {
                epoch: 4,
                ..selector()
            },
            Selector {
                scope_ref: "other".into(),
                ..selector()
            },
            Selector {
                owner: Principal::Direct {},
                ..selector()
            },
        ] {
            let reply = answer(
                json!({"scope": other}),
                Some(&other.owner),
                &[],
                dir.path(),
                &[],
            );
            assert_eq!(reply.data["complete"], true, "{reply:?}");
        }
    }

    #[test]
    fn scope_background_tasks_outstanding_or_failed_write_is_a_named_gap() {
        let dir = tempfile::tempdir().unwrap();
        crate::db::open(&dir.path().join("aft.db")).unwrap();
        let selected = selector();
        let ticket = crate::bash_background::store_writes::Ticket::new(dir.path());
        let observe = || {
            answer(
                json!({"scope": selected}),
                Some(&selected.owner),
                &[],
                dir.path(),
                &[],
            )
        };
        let reply = observe();
        assert_eq!(reply.data["complete"], false);
        assert_eq!(reply.data["reason"], "store_read_failed");
        ticket.resolved();
        assert_eq!(observe().data["complete"], true);
        drop(crate::bash_background::store_writes::Ticket::new(
            dir.path(),
        ));
        let reply = observe();
        assert_eq!(reply.data["complete"], false);
        assert_eq!(reply.data["reason"], "store_read_failed");
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
            &[],
        );
        assert_eq!(reply.data["complete"], false);
        assert_eq!(reply.data["reason"], "registry_not_loaded");
        // Once a DB exists, a matching live route still checks this exact
        // session for a disk-only record, without enumerating other sessions.
        crate::db::open(&dir.path().join("aft.db")).unwrap();
        let routes = answer(
            json!({"scope": selected}),
            Some(&selected.owner),
            &[],
            dir.path(),
            &[task.paths.session_dir],
        );
        assert_eq!(routes.data["complete"], false);
        assert_eq!(routes.data["reason"], "registry_not_loaded");
        let wrong_epoch = Selector {
            epoch: 4,
            ..selector()
        };
        let reply = answer(
            json!({"scope": wrong_epoch}),
            Some(&wrong_epoch.owner),
            &[],
            dir.path(),
            &[],
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
            Path::new("/store"),
        );
        assert_eq!(expected.as_ref().unwrap().roots, vec![root]);
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
            &[],
        );
        assert_eq!(reply.data["complete"], true, "{reply:?}");
        assert_eq!(reply.data["running"][0]["task_id"], id);
        assert_eq!(reply.data["running"][0]["command"], "sleep 30");
        let metrics = Metrics::default();
        let params = json!({"scope": selected});
        let lines = observe_for_test(&metrics, &reply, Some(&selected.owner), &params, true);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("running=1 complete=true outcome=answered"));
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
            &[],
        );
        assert!(start.elapsed() < Duration::from_secs(1));
        assert_eq!(reply.data["complete"], false);
        assert_eq!(reply.data["reason"], "task_busy");
        drop(locked);
        registry.kill(&id, "session").unwrap();
        registry.shutdown();
    }
}
