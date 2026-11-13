use super::super::{
    route_key, send_provider_error, send_reliable_writer_frame, App, DispatchPathMetrics, Executor,
    Frame, FrameType, ManagementIdentity, RouteChannel, SubcError, WriterSender,
};
use super::{admission, state, wire::*};
use crate::bash_background::persistence::BgMode;
use crate::bash_background::registry::{ShellReader, ShellSnapshotCache};
use crate::bash_background::BgTaskStatus;
use crate::db::access_grant_rows::GrantHolder;
use crate::db::access_grants::ScopeKey;
use futures_util::FutureExt;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{watch, Mutex, RwLock};
use tokio_util::sync::CancellationToken;

struct Subscription {
    cancel: CancellationToken,
    done: watch::Receiver<bool>,
}

#[derive(Clone, Copy)]
struct OperationLog {
    op: &'static str,
    caller: &'static str,
    channel: u16,
    started: Instant,
}

impl OperationLog {
    fn open(&self, task_id: Option<&str>) {
        if let Some(task_id) = task_id {
            crate::slog_info!(
                "shells/v1 open: op={} caller={} channel={} elapsed_ms={} task_id={:?}",
                self.op,
                self.caller,
                self.channel,
                self.started.elapsed().as_millis(),
                task_id
            );
        } else {
            crate::slog_info!(
                "shells/v1 open: op={} caller={} channel={} elapsed_ms={}",
                self.op,
                self.caller,
                self.channel,
                self.started.elapsed().as_millis()
            );
        }
    }

    fn end(&self, reason: EndReason) {
        crate::slog_info!(
            "shells/v1 end: op={} caller={} channel={} reason={} elapsed_ms={}",
            self.op,
            self.caller,
            self.channel,
            reason.as_str(),
            self.started.elapsed().as_millis()
        );
    }
}

#[derive(Clone, Copy)]
enum EndReason {
    ClientCancel,
    RouteClosed,
    TaskFinished,
    Error,
}

impl EndReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::ClientCancel => "client_cancel",
            Self::RouteClosed => "route_closed",
            Self::TaskFinished => "task_finished",
            Self::Error => "error",
        }
    }
}

pub(in crate::subc) struct Service {
    app: Arc<App>,
    executor: Arc<Executor>,
    state: RwLock<state::State>,
    sources: RwLock<HashMap<String, ShellReader>>,
    db_facts: RwLock<DbFacts>,
    changed: watch::Sender<u64>,
    refresh_gate: Mutex<()>,
    subscriptions: Mutex<HashMap<(RouteChannel, u64), Subscription>>,
    draining: AtomicBool,
    enabled: AtomicBool,
    stop: CancellationToken,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

impl Service {
    pub(in crate::subc) fn new(app: Arc<App>, executor: Arc<Executor>) -> Arc<Self> {
        let mut epoch = [0u8; 16];
        getrandom::fill(&mut epoch).expect("OS entropy is required for shell watch cursor epochs");
        let service = Arc::new(Self {
            app,
            executor,
            state: RwLock::new(state::State::new(blake3::hash(&epoch).to_hex().to_string())),
            sources: RwLock::new(HashMap::new()),
            db_facts: RwLock::new(DbFacts::default()),
            changed: watch::channel(0).0,
            refresh_gate: Mutex::new(()),
            subscriptions: Mutex::new(HashMap::new()),
            draining: AtomicBool::new(false),
            stop: CancellationToken::new(),
            enabled: AtomicBool::new(false),
        });
        let weak = Arc::downgrade(&service);
        let stop = service.stop.clone();
        let mut changes = crate::bash_background::shell_metadata::subscribe();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            loop {
                let rows = tokio::select! {
                    _ = stop.cancelled() => break,
                    result = changes.changed() => { if result.is_err() { break; } true },
                    _ = tick.tick() => false,
                };
                let Some(service) = weak.upgrade() else {
                    break;
                };
                if !service.enabled.load(Ordering::Acquire) {
                    continue;
                }
                let rows = rows
                    || service
                        .sources
                        .read()
                        .await
                        .values()
                        .any(ShellReader::is_persisted);
                service.refresh(rows).await;
                if rows {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            }
        });
        service
    }

    async fn refresh(&self, rows: bool) {
        let _refresh = self.refresh_gate.lock().await;
        let executor = self.executor.clone();
        let db = self.app.db();
        let previous = self.db_facts.read().await.clone();
        let collected =
            tokio::task::spawn_blocking(move || collect(&executor, db.as_deref(), rows, previous))
                .await;
        let Ok(mut collected) = collected else {
            return;
        };
        *self.db_facts.write().await = collected.db;
        let mut state = self.state.write().await;
        for root in &mut collected.status.roots {
            if root.lsp.is_none() {
                root.lsp = state
                    .status
                    .roots
                    .iter()
                    .find(|old| old.root == root.root)
                    .and_then(|old| old.lsp.clone());
            }
        }
        let before = state.sequence;
        if collected.rows_refreshed && collected.db_available {
            state.replace(collected.rows, collected.status);
            *self.sources.write().await = collected.sources;
        } else {
            state.update_status(collected.status);
        }
        state.expire(now());
        if before != state.sequence {
            self.changed.send_replace(state.sequence);
        }
    }

    pub(in crate::subc) async fn dispatch(
        self: &Arc<Self>,
        tx: &WriterSender,
        metrics: &Arc<DispatchPathMetrics>,
        frame: &Frame,
        identity: &ManagementIdentity,
        body: &Value,
    ) -> Result<(), SubcError> {
        let started = Instant::now();
        let scoped = identity.scope.is_some()
            || matches!(
                identity.admission,
                super::super::ManagementAdmission::ProjectFree { .. }
            );
        let operation = OperationLog {
            op: admission::operation(body),
            caller: admission::caller_kind(
                identity.principal.as_ref(),
                identity.harness.as_deref(),
                scoped,
            ),
            channel: frame.header.channel,
            started,
        };
        let request = match admission::admit(
            identity.principal.as_ref(),
            identity.harness.as_deref(),
            scoped,
            body,
        ) {
            Ok(request) => request,
            Err(error) => {
                admission::log_refusal(operation.op, &error);
                return send_provider_error(tx, metrics, frame, error).await;
            }
        };
        self.enabled.store(true, Ordering::Release);
        match request {
            admission::Request::List(params) => {
                let service = self.clone();
                let tx = tx.clone();
                let metrics = metrics.clone();
                let frame = frame.clone();
                let response_guard = super::super::ResponseTaskGuard::new(&metrics);
                tokio::spawn(async move {
                    let _response = response_guard;
                    service.refresh(true).await;
                    let result = match service.list(params).await {
                        Ok(reply) => {
                            let rows = reply.rows.len();
                            let result = send(
                                &tx,
                                &metrics,
                                &frame,
                                FrameType::Response,
                                &json!({"result":reply}),
                            )
                            .await;
                            crate::slog_info!(
                                "shells/v1 list: caller={} channel={} rows={} elapsed_ms={}",
                                operation.caller,
                                operation.channel,
                                rows,
                                operation.started.elapsed().as_millis()
                            );
                            result
                        }
                        Err(error) => {
                            admission::log_refusal(operation.op, &error);
                            send_provider_error(&tx, &metrics, &frame, error).await
                        }
                    };
                    if result.is_err() {
                        crate::slog_warn!(
                            "shells/v1 send failed: op=shells.list channel={}",
                            operation.channel
                        );
                    }
                });
                Ok(())
            }
            admission::Request::Status { agent_ids } => {
                let service = self.clone();
                let tx = tx.clone();
                let metrics = metrics.clone();
                let frame = frame.clone();
                let response_guard = super::super::ResponseTaskGuard::new(&metrics);
                tokio::spawn(async move {
                    let _response = response_guard;
                    service.refresh(true).await;
                    let status = state::filtered_status(
                        &service.state.read().await.status,
                        agent_ids.as_deref(),
                    );
                    if let Err(error) = send(
                        &tx,
                        &metrics,
                        &frame,
                        FrameType::Response,
                        &json!({"result":status}),
                    )
                    .await
                    {
                        log::warn!("shells/v1 status: {error}");
                    }
                });
                Ok(())
            }
            request => {
                if self.draining.load(Ordering::Acquire) {
                    let result = stream_end(tx, metrics, frame).await;
                    operation.end(if result.is_ok() {
                        EndReason::RouteClosed
                    } else {
                        EndReason::Error
                    });
                    return result;
                }
                let key = (
                    route_key(frame.header.channel, frame.header.epoch),
                    frame.header.corr,
                );
                let cancel = CancellationToken::new();
                let (done, completion) = watch::channel(false);
                let mut subscriptions = self.subscriptions.lock().await;
                if subscriptions.contains_key(&key) {
                    drop(subscriptions);
                    let error = admission::invalid("request correlation is already active");
                    admission::log_refusal(operation.op, &error);
                    return send_provider_error(tx, metrics, frame, error).await;
                }
                subscriptions.insert(
                    key,
                    Subscription {
                        cancel: cancel.clone(),
                        done: completion,
                    },
                );
                drop(subscriptions);
                let service = self.clone();
                let tx = tx.clone();
                let metrics = metrics.clone();
                let frame = frame.clone();
                let route_cancel = identity.shells_cancel.0.clone();
                let response_guard = super::super::ResponseTaskGuard::new(&metrics);
                tokio::spawn(async move {
                    let _response = response_guard;
                    let result = std::panic::AssertUnwindSafe(async {
                        tokio::select! {
                            _ = cancel.cancelled() => return Ok(Some(EndReason::ClientCancel)),
                            _ = route_cancel.cancelled() => return Ok(Some(EndReason::RouteClosed)),
                            _ = service.refresh(true) => {},
                        }
                        match request {
                            admission::Request::Watch(params) => service
                                .watch(
                                    &tx,
                                    &metrics,
                                    &frame,
                                    params,
                                    &cancel,
                                    &route_cancel,
                                    &operation,
                                )
                                .await
                                .map(Some),
                            admission::Request::Output(params) => {
                                service
                                    .output(
                                        &tx,
                                        &metrics,
                                        &frame,
                                        params,
                                        &cancel,
                                        &route_cancel,
                                        &operation,
                                    )
                                    .await
                            }
                            _ => unreachable!(),
                        }
                    })
                    .catch_unwind()
                    .await
                    .unwrap_or_else(|_| Ok(Some(EndReason::Error)));
                    let reason = match result {
                        Ok(Some(EndReason::ClientCancel))
                            if service.draining.load(Ordering::Acquire) =>
                        {
                            Some(EndReason::RouteClosed)
                        }
                        Ok(reason) => reason,
                        Err(_) if route_cancel.is_cancelled() => Some(EndReason::RouteClosed),
                        Err(_) => Some(EndReason::Error),
                    };
                    if let Some(mut reason) = reason {
                        if stream_end(&tx, &metrics, &frame).await.is_err() {
                            reason = EndReason::Error;
                        }
                        operation.end(reason);
                    }
                    done.send_replace(true);
                    service.subscriptions.lock().await.remove(&key);
                });
                Ok(())
            }
        }
    }

    async fn list(&self, params: ListParams) -> Result<ListReply, subc_protocol::ErrorBody> {
        let filter = Filter {
            agent_ids: params.agent_ids.clone(),
            include_workers: params.include_workers,
            cursor: None,
        };
        let state = self.state.read().await;
        let all = state::matching_rows(state.rows.values(), &filter, params.states.as_deref());
        let key = blake3::hash(&serde_json::to_vec(&(&filter, &params.states)).unwrap())
            .to_hex()
            .to_string();
        let prefix = format!("list:{key}:");
        let start = match params.cursor {
            Some(cursor) => cursor
                .strip_prefix(&prefix)
                .and_then(|value| value.parse::<usize>().ok())
                .filter(|start| *start <= all.len())
                .ok_or_else(|| admission::invalid("invalid list cursor"))?,
            None => 0,
        };
        let end = (start + PAGE_SIZE).min(all.len());
        let rows = all[start..end].to_vec();
        let next_cursor = (end < all.len()).then(|| format!("{prefix}{end}"));
        drop(state);
        let unknown_agent_ids = if let Some(agents) = params.agent_ids {
            let db = self.app.db();
            let known = tokio::task::spawn_blocking(move || {
                db.and_then(|db| {
                    db.lock()
                        .ok()
                        .and_then(|conn| crate::db::access_grant_rows::known_agent_ids(&conn).ok())
                })
                .unwrap_or_default()
            })
            .await
            .unwrap_or_default();
            let unknown: Vec<_> = agents
                .into_iter()
                .filter(|id| !known.contains(id))
                .collect();
            (!unknown.is_empty()).then_some(unknown)
        } else {
            None
        };
        Ok(ListReply {
            rows,
            next_cursor,
            unknown_agent_ids,
        })
    }

    async fn watch(
        &self,
        tx: &WriterSender,
        metrics: &DispatchPathMetrics,
        frame: &Frame,
        params: WatchParams,
        cancel: &CancellationToken,
        route: &CancellationToken,
        operation: &OperationLog,
    ) -> Result<EndReason, SubcError> {
        let mut changed = self.changed.subscribe();
        let filter = Filter {
            agent_ids: params.agent_ids,
            include_workers: params.include_workers,
            cursor: None,
        };
        let status = params.status.unwrap_or(false);
        let mut position;
        let initial = {
            let state = self.state.read().await;
            position = params
                .cursor
                .as_deref()
                .and_then(|cursor| state.position(&filter, status, cursor));
            let frames = match position {
                Some(sequence) => state.replay(&filter, status, sequence).unwrap_or_default(),
                None => state.resync(
                    &filter,
                    status,
                    if params.cursor.is_some() {
                        "gap"
                    } else {
                        "initial"
                    },
                ),
            };
            position = Some(state.sequence);
            frames
        };
        let mut opened = false;
        for data in initial {
            send(tx, metrics, frame, FrameType::StreamData, &data).await?;
            if !opened {
                operation.open(None);
                opened = true;
            }
        }
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return Ok(EndReason::ClientCancel),
                _ = route.cancelled() => return Ok(EndReason::RouteClosed),
                result = changed.changed() => if result.is_err() { return Ok(EndReason::Error); },
            }
            let frames = {
                let state = self.state.read().await;
                let frames = state
                    .replay(&filter, status, position.unwrap())
                    .unwrap_or_else(|| {
                        crate::slog_info!(
                            "shells/v1 resync: op=shells.watch caller={} channel={} reason=lag_resync elapsed_ms={}",
                            operation.caller, operation.channel, operation.started.elapsed().as_millis()
                        );
                        state.resync(&filter, status, "lagged")
                    });
                position = Some(state.sequence);
                frames
            };
            for data in frames {
                send(tx, metrics, frame, FrameType::StreamData, &data).await?;
                if !opened {
                    operation.open(None);
                    opened = true;
                }
            }
        }
    }

    pub(in crate::subc) async fn cancel(&self, route: RouteChannel, corr: u64) -> bool {
        let subscription = self.subscriptions.lock().await.remove(&(route, corr));
        let Some(mut subscription) = subscription else {
            return false;
        };
        subscription.cancel.cancel();
        while !*subscription.done.borrow_and_update() {
            if subscription.done.changed().await.is_err() {
                break;
            }
        }
        true
    }

    pub(in crate::subc) async fn drain(&self) {
        self.draining.store(true, Ordering::Release);
        let subscriptions = std::mem::take(&mut *self.subscriptions.lock().await);
        for subscription in subscriptions.values() {
            subscription.cancel.cancel();
        }
        for (_, mut subscription) in subscriptions {
            while !*subscription.done.borrow_and_update() {
                if subscription.done.changed().await.is_err() {
                    break;
                }
            }
        }
    }

    pub(in crate::subc) fn stop(&self) {
        self.stop.cancel();
    }
}

impl Service {
    async fn output(
        &self,
        tx: &WriterSender,
        metrics: &DispatchPathMetrics,
        frame: &Frame,
        params: OutputParams,
        cancel: &CancellationToken,
        route: &CancellationToken,
        operation: &OperationLog,
    ) -> Result<Option<EndReason>, SubcError> {
        let initial = self.state.read().await.rows.get(&params.task_id).cloned();
        let reader = self.sources.read().await.get(&params.task_id).cloned();
        let (Some(row), Some(reader)) = (initial, reader) else {
            let error = subc_protocol::ErrorBody::new("shell_not_found", "shell is not listed");
            admission::log_refusal(operation.op, &error);
            send_provider_error(tx, metrics, frame, error).await?;
            return Ok(None);
        };
        let stop = CancellationToken::new();
        let _stop = stop.clone().drop_guard();
        let task_id = params.task_id.clone();
        let open = tokio::task::spawn_blocking(move || {
            super::capture::Capture::open(reader.with_stop(stop), &row, params)
        });
        let opened = tokio::select! {
            _ = cancel.cancelled() => return Ok(Some(EndReason::ClientCancel)),
            _ = route.cancelled() => return Ok(Some(EndReason::RouteClosed)),
            result = open => result.expect("shell capture reader panicked"),
        };
        let (mut capture, start) = match opened {
            Ok(capture) => capture,
            Err(error) => {
                admission::log_refusal(operation.op, &error);
                send_provider_error(tx, metrics, frame, error).await?;
                return Ok(None);
            }
        };
        send(tx, metrics, frame, FrameType::StreamData, &start).await?;
        operation.open(Some(&task_id));
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return Ok(Some(EndReason::ClientCancel)),
                _ = route.cancelled() => return Ok(Some(EndReason::RouteClosed)),
                _ = tokio::time::sleep(Duration::from_millis(250)) => {},
            }
            let Some(row) = self.state.read().await.rows.get(&task_id).cloned() else {
                return Ok(Some(EndReason::TaskFinished));
            };
            let read = tokio::task::spawn_blocking(move || capture.read(row));
            let (next, frames, finished) = tokio::select! {
                _ = cancel.cancelled() => return Ok(Some(EndReason::ClientCancel)),
                _ = route.cancelled() => return Ok(Some(EndReason::RouteClosed)),
                result = read => result.expect("shell capture reader panicked"),
            };
            capture = next;
            for data in frames {
                send(tx, metrics, frame, FrameType::StreamData, &data).await?;
            }
            if finished {
                return Ok(Some(EndReason::TaskFinished));
            }
        }
    }
}

async fn send(
    tx: &WriterSender,
    metrics: &DispatchPathMetrics,
    request: &Frame,
    ty: FrameType,
    payload: &impl serde::Serialize,
) -> Result<(), SubcError> {
    let body = serde_json::to_vec(payload).map_err(SubcError::Json)?;
    let frame = Frame::build_with_version(
        request.header.ver,
        ty,
        request.header.flags,
        request.header.channel,
        request.header.epoch,
        request.header.corr,
        body,
    )
    .map_err(SubcError::FrameBuild)?;
    send_reliable_writer_frame(tx, metrics, frame, "shells/v1 frame").await
}

async fn stream_end(
    tx: &WriterSender,
    metrics: &DispatchPathMetrics,
    request: &Frame,
) -> Result<(), SubcError> {
    let frame = Frame::build_with_version(
        request.header.ver,
        FrameType::StreamEnd,
        request.header.flags,
        request.header.channel,
        request.header.epoch,
        request.header.corr,
        Vec::new(),
    )
    .map_err(SubcError::FrameBuild)?;
    send_reliable_writer_frame(tx, metrics, frame, "shells/v1 StreamEnd").await
}

fn holder(
    conn: Option<&rusqlite::Connection>,
    metadata: &crate::bash_background::persistence::PersistedTask,
) -> Holder {
    let resolved = metadata.scope.as_ref().and_then(|scope| {
        conn.and_then(|conn| {
            crate::db::access_grant_rows::holder_for_scope(conn, &ScopeKey::from_stamp(scope))
                .ok()
                .flatten()
        })
    });
    resolved_holder(resolved, metadata)
}

fn resolved_holder(
    resolved: Option<GrantHolder>,
    metadata: &crate::bash_background::persistence::PersistedTask,
) -> Holder {
    match resolved {
        Some(GrantHolder::Agent {
            agent_id: Some(agent_id),
        }) => Holder {
            kind: "agent".into(),
            agent_id: Some(agent_id),
            scope_ref: None,
            execution_id: None,
            harness: None,
        },
        Some(GrantHolder::Worker {
            agent_id: Some(agent_id),
            scope_ref,
        }) => Holder {
            kind: "worker".into(),
            agent_id: Some(agent_id),
            scope_ref: Some(scope_ref),
            execution_id: (!metadata.session_id.is_empty()).then(|| metadata.session_id.clone()),
            harness: None,
        },
        _ => Holder {
            kind: "unattributed".into(),
            agent_id: None,
            scope_ref: None,
            execution_id: None,
            harness: metadata.harness.clone(),
        },
    }
}

fn project_row(
    snapshot: &crate::bash_background::registry::ShellSnapshot,
    conn: Option<&rusqlite::Connection>,
) -> Row {
    let metadata = &snapshot.metadata;
    let terminal = metadata.status.is_terminal();
    let remote = metadata.remote.as_ref();
    let phase = remote
        .map(|remote| crate::bash_background::registry::remote::RemotePhase::of(remote).tag());
    let state = if terminal {
        "ended"
    } else if phase.is_some_and(|phase| phase != "running")
        || metadata.status == BgTaskStatus::Starting
    {
        "queued"
    } else {
        "running"
    };
    let command_end = crate::compress::generic::floor_char_boundary(
        &metadata.command,
        metadata.command.len().min(2048),
    );
    let end_reason = terminal.then(|| match metadata.status {
        BgTaskStatus::FateUnknown => "lost",
        BgTaskStatus::TimedOut => "timed_out",
        BgTaskStatus::Killed if metadata.status_reason.as_deref().is_some_and(|reason| reason == crate::bash_background::registry::folder_grants::FOLDER_ACCESS_REVOKED_REASON) => "folder_access_revoked",
        BgTaskStatus::Killed if crate::bash_background::registry::scope_end_label(metadata.status_reason.as_deref()).is_some() => "cancelled",
        BgTaskStatus::Killed => "killed",
        _ => "exited",
    }.to_owned());
    Row {
        task_id: metadata.task_id.clone(),
        revision: 0,
        holder: holder(conn, metadata),
        session: metadata
            .shell_call
            .as_ref()
            .map(|call| call.session.clone()),
        call_id: metadata
            .shell_call
            .as_ref()
            .and_then(|call| call.call_id.clone()),
        title: metadata
            .shell_call
            .as_ref()
            .and_then(|call| call.description.clone()),
        command: metadata.command[..command_end].to_owned(),
        command_truncated: (command_end < metadata.command.len()).then_some(true),
        cwd: metadata.workdir.display().to_string(),
        kind: if remote.is_some() && !metadata.local_fallback_started {
            "remote".into()
        } else if metadata.mode == BgMode::Pty {
            "pty".into()
        } else {
            metadata
                .shell_call
                .as_ref()
                .map(|call| call.kind.clone())
                .unwrap_or_else(|| "background".into())
        },
        remote: remote.and_then(|remote| {
            remote
                .job_id
                .map(|job_id| job_id.to_string())
                .or_else(|| metadata.shell_remote_job_id.clone())
                .map(|job_id| Remote {
                    job_id,
                    host: metadata.shell_remote_host.clone(),
                    phase: phase.unwrap_or("queued").into(),
                })
        }),
        state: state.into(),
        started_at_ms: metadata.started_at,
        last_output_at_ms: snapshot.last_output_at_ms,
        ended_at_ms: metadata.finished_at,
        output_bytes: Offsets {
            stdout: snapshot.stdout,
            stderr: snapshot.stderr,
        },
        exit: metadata.exit_code.map(|code| Exit {
            code: Some(code),
            signal: None,
        }),
        end_reason,
    }
}

fn readiness(value: &Value) -> Readiness {
    let code = value
        .get("status")
        .or_else(|| value.get("state"))
        .and_then(Value::as_str)
        .unwrap_or("loading");
    let state = match code {
        "ready" | "available" | "running" => "ready",
        "off" | "disabled" | "not_applicable" => "disabled",
        "building" | "loading" | "pending" | "busy" | "starting" => "building",
        _ => "degraded",
    };
    let reason = value
        .get("reason")
        .and_then(Value::as_str)
        .filter(|reason| {
            reason
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
        })
        .map(str::to_owned)
        .or_else(|| (state != "ready").then(|| code.to_owned()));
    Readiness {
        state: state.into(),
        reason,
    }
}

#[derive(Clone, Default)]
struct DbFacts {
    key: Option<FactsKey>,
    root_agents: BTreeMap<String, Vec<String>>,
    truncated_roots: std::collections::BTreeSet<String>,
    holders: HashMap<ScopeKey, GrantHolder>,
    saved: Vec<crate::db::bash_tasks::BashTaskRow>,
    saved_key: Option<(usize, u64, i64)>,
    samples: ShellSnapshotCache,
}

#[derive(Clone, PartialEq, Eq)]
struct FactsKey {
    revision: u64,
    roots: std::collections::BTreeSet<String>,
    db_identity: Option<usize>,
}

// Root-agent associations are sidebar hints, not an exhaustive agent inventory.
// One lookahead row identifies the first root whose list may be incomplete.
const ROOT_ASSOCIATION_LIMIT: usize = 256;

struct Collected {
    rows: Vec<Row>,
    status: Status,
    sources: HashMap<String, ShellReader>,
    db: DbFacts,
    db_available: bool,
    rows_refreshed: bool,
}

#[cfg(test)]
thread_local! {
    static AFTER_DB_FACTS: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
thread_local! {
    static DB_LOCK_DURATIONS: std::cell::RefCell<Vec<std::time::Duration>> = const {std::cell::RefCell::new(Vec::new())};
}

#[cfg(test)]
static COLLECT_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn with_db_read<T>(
    db: &std::sync::Mutex<crate::db::TrackedConnection>,
    read: impl FnOnce(&rusqlite::Connection) -> rusqlite::Result<T>,
) -> Option<T> {
    let conn = db.try_lock().ok()?;
    #[cfg(test)]
    let started = std::time::Instant::now();
    let result = match read(&conn) {
        Ok(value) => Some(value),
        Err(error) => {
            #[cfg(test)]
            eprintln!("shell facts SQL failed: {error}");
            #[cfg(not(test))]
            let _ = error;
            None
        }
    };
    drop(conn);
    #[cfg(test)]
    DB_LOCK_DURATIONS.with(|samples| samples.borrow_mut().push(started.elapsed()));
    result
}

fn read_root_agents(
    conn: &rusqlite::Connection,
    roots: &std::collections::BTreeSet<String>,
    facts: &mut DbFacts,
) -> rusqlite::Result<()> {
    if roots.is_empty() {
        return Ok(());
    }
    let roots_json = serde_json::to_string(roots).expect("root strings serialize");
    // The launch primary key starts with project_root for indexed root seeks.
    // SQLite applies the unique-association limit before returning rows; Rust
    // never materializes the entire association list just to truncate it.
    let mut query=conn.prepare("SELECT DISTINCT l.project_root,
        CASE a.kind WHEN 'head' THEN NULLIF(a.agent_id,'') WHEN 'worker' THEN COALESCE(
          (SELECT h.agent_id FROM access_scope_agents h
           WHERE h.owner=s.parent_owner AND h.scope_ref=s.parent AND h.epoch=s.parent_epoch AND h.kind='head' AND h.agent_id<>''),
          (SELECT h.agent_id FROM access_scope_agents h
           WHERE h.owner=s.parent_owner AND h.scope_ref=s.parent AND h.kind='head' AND h.agent_id<>''
           ORDER BY length(h.epoch) DESC,h.epoch DESC LIMIT 1)
        ) END AS head_id
        FROM access_launch_params l
        JOIN access_scopes s USING(owner,scope_ref,epoch)
        JOIN access_scope_agents a USING(owner,scope_ref,epoch)
        WHERE l.project_root IN (SELECT value FROM json_each(?1)) AND s.ended=0
          AND a.kind IN ('head','worker') AND head_id IS NOT NULL
        ORDER BY l.project_root LIMIT ?2")?;
    let entries = query.query_map(
        rusqlite::params![roots_json, (ROOT_ASSOCIATION_LIMIT + 1) as i64],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
    )?;
    for (index, entry) in entries.enumerate() {
        let (root, agent) = entry?;
        if index == ROOT_ASSOCIATION_LIMIT {
            // Earlier roots were fully visited. This root and all later roots
            // share the exhausted budget and must not claim complete lists.
            facts.truncated_roots.extend(roots.range(root..).cloned());
        } else {
            facts.root_agents.entry(root).or_default().push(agent);
        }
    }
    Ok(())
}

fn read_db_facts(
    db: Option<&std::sync::Mutex<crate::db::TrackedConnection>>,
    scopes: &std::collections::HashSet<ScopeKey>,
    roots: &std::collections::BTreeSet<String>,
    mut previous: DbFacts,
) -> (DbFacts, bool) {
    let Some(db) = db else {
        return (previous, true);
    };
    // Only scopes observed on live or retained shell rows need a holder. No
    // context, registry, file or language-server lock is held in this closure.
    let loaded = with_db_read(db, |conn| {
        let mut facts = DbFacts::default();
        for key in scopes {
            if let Some(holder) = crate::db::access_grant_rows::holder_for_scope(conn, key)? {
                facts.holders.insert(key.clone(), holder);
            }
        }
        read_root_agents(conn, roots, &mut facts)?;
        Ok(facts)
    });
    if let Some(mut facts) = loaded {
        facts.saved = std::mem::take(&mut previous.saved);
        facts.saved_key = previous.saved_key;
        facts.samples = previous.samples;
        (facts, true)
    } else {
        (previous, false)
    }
}

fn collect(
    executor: &Executor,
    db: Option<&std::sync::Mutex<crate::db::TrackedConnection>>,
    include_rows: bool,
    previous: DbFacts,
) -> Collected {
    collect_at_revision(
        executor,
        db,
        include_rows,
        previous,
        crate::bash_background::shell_metadata::facts_revision(),
    )
}

/// Shared project facts for shell status and the person's status command. No
/// database guard crosses this call while it reads language-server diagnostics.
pub(in crate::subc) fn project_status(ctx: &crate::context::AppContext) -> (Value, Option<usize>) {
    let snapshot = ctx.build_operator_status_snapshot();
    let running = ctx
        .bash_background()
        .try_health_counts()
        .map(|counts| counts.running);
    (snapshot, running)
}

fn collect_at_revision(
    executor: &Executor,
    db: Option<&std::sync::Mutex<crate::db::TrackedConnection>>,
    include_rows: bool,
    previous: DbFacts,
    revision: u64,
) -> Collected {
    collect_at_store_revision(
        executor,
        db,
        include_rows,
        previous,
        revision,
        crate::db::bash_tasks::store_revision(),
    )
}

fn collect_at_store_revision(
    executor: &Executor,
    db: Option<&std::sync::Mutex<crate::db::TrackedConnection>>,
    include_rows: bool,
    previous: DbFacts,
    revision: u64,
    generation: u64,
) -> Collected {
    #[cfg(test)]
    COLLECT_CALLS.fetch_add(1, Ordering::Relaxed);
    let contexts = executor.actor_contexts();
    let bound_roots: std::collections::BTreeSet<_> = contexts
        .iter()
        .filter_map(|ctx| {
            ctx.config()
                .project_root
                .as_ref()
                .map(|root| root.display().to_string())
        })
        .collect();
    let key = FactsKey {
        revision,
        roots: bound_roots.clone(),
        db_identity: db.map(|db| std::ptr::from_ref(db) as usize),
    };
    let mut dirty = previous.key.as_ref() != Some(&key);
    let mut facts = previous;
    let mut db_available = true;
    if let Some(db) = db {
        if let Some((saved_key, saved)) = with_db_read(db, |conn| {
            // data_version catches committed writes from other connections
            // and processes; the process generation covers this connection's
            // own writes, which SQLite excludes from data_version.
            let version: i64 = conn.query_row("PRAGMA data_version", [], |row| row.get(0))?;
            let saved_key = (std::ptr::from_ref(db) as usize, generation, version);
            let saved = if facts.saved_key == Some(saved_key) {
                None
            } else {
                Some(crate::db::bash_tasks::operator_shell_rows(
                    conn,
                    now().saturating_sub(RETENTION_MS),
                )?)
            };
            Ok((saved_key, saved))
        }) {
            if let Some(saved) = saved {
                facts.saved = saved;
                dirty = true;
            }
            facts.saved_key = Some(saved_key);
        } else {
            db_available = false;
        }
    }
    let observe = db_available && (include_rows || dirty);
    let mut observations = Vec::new();
    let mut sources = HashMap::new();
    for ctx in &contexts {
        if observe {
            let registry = ctx.bash_background();
            for snapshot in registry.shell_snapshots_cached(&mut facts.samples) {
                if !state::retained(&project_row(&snapshot, None), now()) {
                    continue;
                }
                if let Some(reader) = registry.shell_reader(&snapshot.metadata.task_id) {
                    sources.insert(snapshot.metadata.task_id.clone(), reader);
                }
                observations.push(snapshot);
            }
        }
    }
    if observe {
        for saved in facts.saved.iter().cloned() {
            if sources.contains_key(&saved.task_id) {
                continue;
            }
            if let Some((snapshot, reader)) =
                ShellReader::persisted_cached(saved, &mut facts.samples)
            {
                if !state::retained(&project_row(&snapshot, None), now()) {
                    continue;
                }
                sources.insert(snapshot.metadata.task_id.clone(), reader);
                observations.push(snapshot);
            }
        }
    }
    if observe {
        facts.samples.retain(&sources);
    }
    if dirty && db_available {
        let scopes = observations
            .iter()
            .filter_map(|snapshot| snapshot.metadata.scope.as_ref().map(ScopeKey::from_stamp))
            .collect();
        (facts, db_available) = read_db_facts(db, &scopes, &bound_roots, facts);
        if db_available {
            facts.key = Some(key);
        }
    }
    #[cfg(test)]
    AFTER_DB_FACTS.with(|hook| {
        if let Some(hook) = hook.borrow_mut().take() {
            hook();
        }
    });
    let mut rows = Vec::new();
    if observe && db_available {
        for snapshot in observations {
            let mut row = project_row(&snapshot, None);
            row.revision = snapshot.metadata.shell_revision;
            row.holder = resolved_holder(
                snapshot
                    .metadata
                    .scope
                    .as_ref()
                    .and_then(|scope| facts.holders.get(&ScopeKey::from_stamp(scope)))
                    .cloned(),
                &snapshot.metadata,
            );
            rows.push(row);
        }
    }
    let mut root_agents = facts.root_agents.clone();
    root_agents.retain(|root, _| bound_roots.contains(root));
    for row in &mut rows {
        let mut fingerprint_row = row.clone();
        fingerprint_row.revision = 0;
        let fingerprint = blake3::hash(&serde_json::to_vec(&fingerprint_row).unwrap())
            .to_hex()
            .to_string();
        if let Some(registry) = sources.get(&row.task_id) {
            row.revision = registry
                .shell_revision_if_changed(&row.task_id, &fingerprint)
                .unwrap_or(row.revision);
        }
    }
    let mut roots = Vec::new();
    let mut degraded = false;
    let mut building = false;
    for ctx in contexts {
        let (snapshot, _) = project_status(&ctx);
        let Some(root) = snapshot["project_root"].as_str() else {
            continue;
        };
        let mut agents = root_agents.remove(root).unwrap_or_default();
        agents.sort();
        agents.dedup();
        let text_index = readiness(&snapshot["search_index"]);
        let call_graph = if snapshot["callgraph_store"].is_null()
            && snapshot["features"]["callgraph_store"] == false
        {
            Readiness {
                state: "disabled".into(),
                reason: Some("configured_disabled".into()),
            }
        } else {
            readiness(&snapshot["callgraph_store"])
        };
        let meaning = readiness(&snapshot["semantic_index"]);
        degraded |= snapshot["degraded"].as_bool().unwrap_or(false)
            || [&text_index, &call_graph, &meaning]
                .iter()
                .any(|plane| plane.state == "degraded");
        building |= [&text_index, &call_graph, &meaning]
            .iter()
            .any(|plane| plane.state == "building");
        let lsp = ctx
            .try_lsp()
            .map(|manager| manager.operator_server_states());
        let lsp = lsp.map(|servers| {
            let mut lsp: Vec<_> = servers
                .into_iter()
                .map(|(server, state, reason)| Lsp {
                    server,
                    state,
                    reason,
                })
                .collect();
            let config = ctx.config();
            for server in config.disabled_lsp.iter().chain(
                config
                    .lsp_servers
                    .iter()
                    .filter(|server| server.disabled)
                    .map(|server| &server.id),
            ) {
                lsp.retain(|entry| &entry.server != server);
                lsp.push(Lsp {
                    server: server.clone(),
                    state: "disabled".into(),
                    reason: Some("configured_disabled".into()),
                });
            }
            lsp.sort_by(|a, b| a.server.cmp(&b.server));
            lsp
        });
        roots.push(RootStatus {
            root: root.into(),
            agent_ids: agents,
            agent_ids_truncated: facts.truncated_roots.contains(root).then_some(true),
            text_index,
            call_graph,
            meaning,
            lsp,
            errors: snapshot["status_bar_values"]["errors"].as_u64(),
            warnings: snapshot["status_bar_values"]["warnings"].as_u64(),
            todos: snapshot["status_bar_values"]["todos"].as_u64(),
        });
    }
    for (root, mut agents) in root_agents {
        agents.sort();
        agents.dedup();
        let readiness = Readiness {
            state: if bound_roots.contains(&root) {
                "building"
            } else {
                "disabled"
            }
            .into(),
            reason: Some(
                if bound_roots.contains(&root) {
                    "status_busy"
                } else {
                    "root_not_bound"
                }
                .into(),
            ),
        };
        roots.push(RootStatus {
            agent_ids_truncated: facts.truncated_roots.contains(&root).then_some(true),
            root,
            agent_ids: agents,
            text_index: readiness.clone(),
            call_graph: readiness.clone(),
            meaning: readiness,
            lsp: None,
            errors: None,
            warnings: None,
            todos: None,
        });
    }
    roots.sort_by(|a, b| a.root.cmp(&b.root));
    Collected {
        rows,
        status: Status {
            aft: Aft {
                version: env!("CARGO_PKG_VERSION").into(),
                card: if degraded {
                    "degraded"
                } else if building {
                    "building"
                } else {
                    "ready"
                }
                .into(),
            },
            roots,
        },
        sources,
        db: facts,
        db_available,
        rows_refreshed: observe,
    }
}

#[cfg(test)]
#[path = "runtime_tests.rs"]
mod tests;
