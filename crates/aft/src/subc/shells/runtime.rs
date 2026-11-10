use super::super::{
    route_key, send_provider_error, send_reliable_writer_frame, App, DispatchPathMetrics, Executor,
    Frame, FrameType, ManagementIdentity, RouteChannel, SubcError, WriterSender,
};
use super::{admission, output::Chunker, state, wire::*};
use crate::bash_background::persistence::{BgMode, TaskArtifact};
use crate::bash_background::registry::ShellReader;
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
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{watch, Mutex, RwLock};
use tokio_util::sync::CancellationToken;

struct Subscription {
    cancel: CancellationToken,
    done: watch::Receiver<bool>,
}

pub(in crate::subc) struct Service {
    app: Arc<App>,
    executor: Arc<Executor>,
    state: RwLock<state::State>,
    sources: RwLock<HashMap<String, ShellReader>>,
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
        let service = Arc::new(Self {
            app,
            executor,
            state: RwLock::new(state::State::new(uuid::Uuid::new_v4().to_string())),
            sources: RwLock::new(HashMap::new()),
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
        let collected =
            tokio::task::spawn_blocking(move || collect(&executor, db.as_deref(), rows)).await;
        let Ok((incoming, status, sources)) = collected else {
            return;
        };
        let mut state = self.state.write().await;
        let before = state.sequence;
        if rows {
            state.replace(incoming, status);
            *self.sources.write().await = sources;
        } else {
            state.update_status(status);
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
        let scoped = identity.scope.is_some()
            || matches!(
                identity.admission,
                super::super::ManagementAdmission::ProjectFree { .. }
            );
        let request = match admission::admit(
            identity.principal.as_ref(),
            identity.harness.as_deref(),
            scoped,
            body,
        ) {
            Ok(request) => request,
            Err(error) => return send_provider_error(tx, metrics, frame, error).await,
        };
        self.enabled.store(true, Ordering::Release);
        match request {
            admission::Request::List(params) => {
                let service = self.clone();
                let tx = tx.clone();
                let metrics = metrics.clone();
                let frame = frame.clone();
                tokio::spawn(async move {
                    let _response = super::super::ResponseTaskGuard::new(&metrics);
                    service.refresh(true).await;
                    let result = match service.list(params).await {
                        Ok(reply) => {
                            send(
                                &tx,
                                &metrics,
                                &frame,
                                FrameType::Response,
                                &json!({"result":reply}),
                            )
                            .await
                        }
                        Err(error) => send_provider_error(&tx, &metrics, &frame, error).await,
                    };
                    if let Err(error) = result {
                        log::warn!("shells/v1 list: {error}");
                    }
                });
                Ok(())
            }
            admission::Request::Status { agent_ids } => {
                let service = self.clone();
                let tx = tx.clone();
                let metrics = metrics.clone();
                let frame = frame.clone();
                tokio::spawn(async move {
                    let _response = super::super::ResponseTaskGuard::new(&metrics);
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
                    return stream_end(tx, metrics, frame).await;
                }
                let key = (
                    route_key(frame.header.channel, frame.header.epoch),
                    frame.header.corr,
                );
                let cancel = CancellationToken::new();
                let (done, completion) = watch::channel(false);
                self.subscriptions.lock().await.insert(
                    key,
                    Subscription {
                        cancel: cancel.clone(),
                        done: completion,
                    },
                );
                let service = self.clone();
                let tx = tx.clone();
                let metrics = metrics.clone();
                let frame = frame.clone();
                let route_cancel = identity.shells_cancel.clone();
                tokio::spawn(async move {
                    let _response = super::super::ResponseTaskGuard::new(&metrics);
                    let result = std::panic::AssertUnwindSafe(async {
                        service.refresh(true).await;
                        match request {
                            admission::Request::Watch(params) => service
                                .watch(&tx, &metrics, &frame, params, &cancel, &route_cancel)
                                .await
                                .map(|_| true),
                            admission::Request::Output(params) => {
                                service
                                    .output(&tx, &metrics, &frame, params, &cancel, &route_cancel)
                                    .await
                            }
                            _ => unreachable!(),
                        }
                    })
                    .catch_unwind()
                    .await
                    .unwrap_or_else(|_| {
                        log::warn!("shells/v1 stream panicked; ending its request");
                        Ok(true)
                    });
                    let end = !matches!(result, Ok(false));
                    if let Err(error) = result {
                        log::warn!("shells/v1 stream: {error}");
                    }
                    if end {
                        if let Err(error) = stream_end(&tx, &metrics, &frame).await {
                            log::warn!("shells/v1 terminal: {error}");
                        }
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
            let known = db
                .and_then(|db| {
                    db.lock()
                        .ok()
                        .and_then(|conn| crate::db::access_grant_rows::known_agent_ids(&conn).ok())
                })
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
    ) -> Result<(), SubcError> {
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
        for data in initial {
            send(tx, metrics, frame, FrameType::StreamData, &data).await?;
        }
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return Ok(()),
                _ = route.cancelled() => return Ok(()),
                result = changed.changed() => if result.is_err() { return Ok(()); },
            }
            let frames = {
                let state = self.state.read().await;
                let frames = state
                    .replay(&filter, status, position.unwrap())
                    .unwrap_or_else(|| state.resync(&filter, status, "lagged"));
                position = Some(state.sequence);
                frames
            };
            for data in frames {
                send(tx, metrics, frame, FrameType::StreamData, &data).await?;
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
    ) -> Result<bool, SubcError> {
        let initial = self.state.read().await.rows.get(&params.task_id).cloned();
        let registry = self.sources.read().await.get(&params.task_id).cloned();
        let (Some(mut row), Some(registry)) = (initial, registry) else {
            send_provider_error(
                tx,
                metrics,
                frame,
                subc_protocol::ErrorBody::new("shell_not_found", "shell is not listed"),
            )
            .await?;
            return Ok(false);
        };
        let max = params.max_chunk_bytes.unwrap_or(DEFAULT_CHUNK_BYTES);
        let ansi = params.ansi.unwrap_or_else(|| "strip".into());
        let pty = row.kind == "pty";
        let stdout_artifact = if pty {
            TaskArtifact::Pty
        } else {
            TaskArtifact::Stdout
        };
        let mut starts = Offsets {
            stdout: super::output::backfill_start(
                row.output_bytes.stdout,
                params.from_offset.as_ref().map(|offset| offset.stdout),
            ),
            stderr: super::output::backfill_start(
                row.output_bytes.stderr,
                params.from_offset.as_ref().map(|offset| offset.stderr),
            ),
        };
        if params.from_offset.as_ref().is_some_and(|offset| {
            offset.stdout > row.output_bytes.stdout || offset.stderr > row.output_bytes.stderr
        }) {
            send_provider_error(
                tx,
                metrics,
                frame,
                admission::invalid("from_offset exceeds the captured stream"),
            )
            .await?;
            return Ok(false);
        }
        for (artifact, offset) in [
            (stdout_artifact, &mut starts.stdout),
            (TaskArtifact::Stderr, &mut starts.stderr),
        ] {
            if *offset > 0 {
                let edge = registry
                    .shell_read(&params.task_id, artifact, *offset, 4)
                    .unwrap_or_default();
                let continuation = edge.iter().take_while(|byte| **byte & 0xc0 == 0x80).count();
                if continuation > 0 && params.from_offset.is_some() {
                    send_provider_error(
                        tx,
                        metrics,
                        frame,
                        admission::invalid("from_offset must be a UTF-8 boundary"),
                    )
                    .await?;
                    return Ok(false);
                }
                *offset += continuation as u64;
            }
        }
        let mut stdout = Chunker::new(starts.stdout, max, ansi == "keep");
        let mut stderr = Chunker::new(starts.stderr, max, ansi == "keep");
        for (artifact, chunker) in [
            (stdout_artifact, &mut stdout),
            (TaskArtifact::Stderr, &mut stderr),
        ] {
            if chunker.offset() > 0 {
                let prefix = registry
                    .shell_read(&params.task_id, artifact, 0, chunker.offset())
                    .unwrap_or_default();
                chunker.resume_escape(&prefix);
            }
        }
        let truncated_before = (starts.stdout > 0 || starts.stderr > 0).then_some(starts);
        send(
            tx,
            metrics,
            frame,
            FrameType::StreamData,
            &OutputFrame::Start {
                task_id: params.task_id.clone(),
                ansi,
                truncated_before,
            },
        )
        .await?;
        let mut screen_at = tokio::time::Instant::now() - Duration::from_secs(1);
        let mut previous_screen = None;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return Ok(true),
                _ = route.cancelled() => return Ok(true),
                _ = tokio::time::sleep(Duration::from_millis(250)) => {},
            }
            if let Some(current) = self.state.read().await.rows.get(&params.task_id).cloned() {
                row = current;
            } else {
                return Ok(true);
            }
            let terminal = row.state == "ended";
            let longest = registry.shell_longest_secret(&params.task_id);
            for (stream, artifact, chunker) in [
                ("stdout", stdout_artifact, &mut stdout),
                ("stderr", TaskArtifact::Stderr, &mut stderr),
            ] {
                let pending = chunker.read_offset().saturating_sub(chunker.offset()) as usize;
                let want = (max + longest + 8).saturating_sub(pending);
                let bytes = registry
                    .shell_read(
                        &params.task_id,
                        artifact,
                        chunker.read_offset(),
                        want as u64,
                    )
                    .unwrap_or_default();
                chunker.append(&bytes);
                let length = if stream == "stdout" {
                    row.output_bytes.stdout
                } else {
                    row.output_bytes.stderr
                };
                let at_end = terminal && chunker.read_offset() >= length;
                if let Some(data) = chunker
                    .next(stream, at_end, longest, |from, to| {
                        Ok::<_, std::convert::Infallible>(registry.shell_masked_range(
                            &params.task_id,
                            artifact,
                            from,
                            to,
                        ))
                    })
                    .unwrap()
                {
                    send(tx, metrics, frame, FrameType::StreamData, &data).await?;
                }
            }
            if pty && screen_at.elapsed() >= Duration::from_millis(500) {
                if let Some((rows, cols)) = registry.shell_dimensions(&params.task_id) {
                    let end = if terminal {
                        stdout.read_offset()
                    } else {
                        stdout.read_offset().saturating_sub(longest as u64)
                    };
                    let mut masked = String::new();
                    let mut from = 0;
                    while from < end {
                        let to = (from + DEFAULT_CHUNK_BYTES as u64).min(end);
                        masked.push_str(&registry.shell_masked_range(
                            &params.task_id,
                            stdout_artifact,
                            from,
                            to,
                        ));
                        from = to;
                    }
                    let lines = crate::pty_render::render_screen(masked.as_bytes(), rows, cols)
                        .split('\n')
                        .map(str::to_owned)
                        .collect();
                    let mut parser = vt100::Parser::new(rows, cols, 0);
                    parser.process(masked.as_bytes());
                    let (cursor_row, cursor_col) = parser.screen().cursor_position();
                    let screen = OutputFrame::Screen {
                        rows: rows as usize,
                        cols: cols as usize,
                        lines,
                        cursor: ScreenCursor {
                            row: cursor_row as usize,
                            col: cursor_col as usize,
                        },
                    };
                    if previous_screen.as_ref() != Some(&screen) {
                        send(tx, metrics, frame, FrameType::StreamData, &screen).await?;
                        previous_screen = Some(screen);
                    }
                }
                screen_at = tokio::time::Instant::now();
            }
            if terminal
                && stdout.offset() >= row.output_bytes.stdout
                && stderr.offset() >= row.output_bytes.stderr
            {
                send(
                    tx,
                    metrics,
                    frame,
                    FrameType::StreamData,
                    &OutputFrame::End { row },
                )
                .await?;
                return Ok(true);
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
                    host: None,
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

fn collect(
    executor: &Executor,
    db: Option<&std::sync::Mutex<crate::db::TrackedConnection>>,
    include_rows: bool,
) -> (Vec<Row>, Status, HashMap<String, ShellReader>) {
    let contexts = executor.actor_contexts();
    let db_guard = db.and_then(|db| db.lock().ok());
    let conn = db_guard.as_deref().map(|conn| &**conn);
    let mut rows = Vec::new();
    let mut sources = HashMap::new();
    let mut root_agents = BTreeMap::<String, Vec<String>>::new();
    if let Some(conn) = conn {
        if let Ok(mut statement) = conn
            .prepare("SELECT DISTINCT project_root,owner,scope_ref,epoch FROM access_launch_params")
        {
            if let Ok(entries) = statement.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            }) {
                for entry in entries.flatten() {
                    let (root, owner, scope_ref, epoch) = entry;
                    let Ok(epoch) = epoch.parse() else {
                        continue;
                    };
                    if let Ok(Some(holder)) = crate::db::access_grant_rows::holder_for_scope(
                        conn,
                        &ScopeKey {
                            owner,
                            scope_ref,
                            epoch,
                        },
                    ) {
                        if let Some(agent) = holder.agent_id() {
                            root_agents.entry(root).or_default().push(agent.to_owned());
                        }
                    }
                }
            }
        }
    }
    for ctx in &contexts {
        let root = ctx
            .config()
            .project_root
            .as_ref()
            .map(|root| root.display().to_string())
            .unwrap_or_default();
        if include_rows {
            let registry = ctx.bash_background();
            for snapshot in registry.shell_snapshots() {
                let mut row = project_row(&snapshot, conn);
                if !state::retained(&row, now()) {
                    continue;
                }
                if let Some(agent) = &row.holder.agent_id {
                    root_agents
                        .entry(root.clone())
                        .or_default()
                        .push(agent.clone());
                }
                row.revision = snapshot.metadata.shell_revision;
                if let Some(reader) = registry.shell_reader(&row.task_id) {
                    sources.insert(row.task_id.clone(), reader);
                }
                rows.push(row);
            }
        }
    }
    if include_rows {
        if let Some(conn) = conn {
            for saved in
                crate::db::bash_tasks::operator_shell_rows(conn, now().saturating_sub(RETENTION_MS))
                    .unwrap_or_default()
            {
                if sources.contains_key(&saved.task_id) {
                    continue;
                }
                if let Some((snapshot, reader)) = ShellReader::persisted(saved) {
                    let mut row = project_row(&snapshot, Some(conn));
                    if !state::retained(&row, now()) {
                        continue;
                    }
                    if let Some(agent) = &row.holder.agent_id {
                        root_agents
                            .entry(
                                snapshot
                                    .metadata
                                    .project_root
                                    .as_ref()
                                    .unwrap_or(&snapshot.metadata.workdir)
                                    .display()
                                    .to_string(),
                            )
                            .or_default()
                            .push(agent.clone());
                    }
                    row.revision = snapshot.metadata.shell_revision;
                    sources.insert(row.task_id.clone(), reader);
                    rows.push(row);
                }
            }
        }
    }
    drop(db_guard);
    for row in &mut rows {
        let mut fingerprint_row = row.clone();
        fingerprint_row.revision = 0;
        let fingerprint = blake3::hash(&serde_json::to_vec(&fingerprint_row).unwrap())
            .to_hex()
            .to_string();
        if let Some(registry) = sources.get(&row.task_id) {
            row.revision = registry
                .shell_revision(&row.task_id, &fingerprint)
                .unwrap_or(row.revision);
        }
    }
    let bound_roots: std::collections::BTreeSet<_> = contexts
        .iter()
        .filter_map(|ctx| {
            ctx.config()
                .project_root
                .as_ref()
                .map(|root| root.display().to_string())
        })
        .collect();
    let mut roots = Vec::new();
    let mut degraded = false;
    let mut building = false;
    for ctx in contexts {
        let Some(snapshot) = ctx.try_build_status_push_snapshot() else {
            continue;
        };
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
        let mut lsp: Vec<_> = ctx
            .lsp()
            .operator_server_states()
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
        roots.push(RootStatus {
            root: root.into(),
            agent_ids: agents,
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
            root,
            agent_ids: agents,
            text_index: readiness.clone(),
            call_graph: readiness.clone(),
            meaning: readiness,
            lsp: vec![],
            errors: None,
            warnings: None,
            todos: None,
        });
    }
    roots.sort_by(|a, b| a.root.cmp(&b.root));
    (
        rows,
        Status {
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
    )
}

#[cfg(test)]
#[path = "runtime_tests.rs"]
mod tests;
