//! Remote route ingress and its audit → preflight → consent → dispatch sequence.

use super::{dispatch::FileTool, record::Record, *};
use crate::db::call_ledger::{self as ledger, RecordedFrame};
use cortexkit_role_tool_provider::{call::ToolCallRequest, catalog::composition_digest, errors};
use remote_bash::{Running, Shell};
use scope_describe::remote::{self as scope_lifetime, Lifetime};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tool_provider::{remote_pins, CatalogPreset};

#[path = "remote_bash_service.rs"]
mod bash_service;

#[derive(PartialEq, Eq)]
pub(in crate::subc) struct Binding {
    session: String,
    held_reply: bool,
    held_update: bool,
    tool_result_body: bool,
    call_yield: bool,
}

impl Binding {
    pub(in crate::subc) fn new(session: String, caps: Option<&[String]>) -> Self {
        Self {
            session,
            held_reply: caps.is_some_and(|caps| caps.iter().any(|cap| cap == "held-reply/v1")),
            held_update: cortexkit_role_tool_provider::consumer_reads_held_update_v1(caps),
            tool_result_body: result_body::accepts(caps),
            call_yield: call_yield::consumer_reads_call_yield(caps),
        }
    }

    fn check_transport(&self, preset: CatalogPreset) -> Result<(), ErrorBody> {
        let (late_results, bash_yield) = remote_pins::transport(preset);
        if self.held_reply
            && self.tool_result_body
            && late_results
            && bash_yield.is_none_or(|tagged| self.call_yield && tagged)
        {
            return Ok(());
        }
        Err(admission_refusal())
    }
}

fn carrier(principal: &str, scope: &ScopeStamp) -> String {
    let facts = scope.attributes.federated.as_ref().expect("admitted scope");
    let stamp = tool_provider::ledger_scope(scope);
    // Use a structured spelling so adjacent fields cannot alias, and include
    // the stamp tuple so different agents on one computer don't share a feed.
    format!(
        "remote#{}",
        json!([
            principal,
            facts.pairing_id,
            stamp.owner,
            stamp.scope_ref,
            stamp.scope_epoch
        ])
    )
}

struct Edge {
    writer: WriterSender,
    metrics: Arc<DispatchPathMetrics>,
    app: Arc<App>,
    database: PathBuf,
    executor: Arc<Executor>,
    dispatch: DispatchFn,
    max_body: usize,
    #[cfg(all(test, target_os = "linux"))]
    test_launcher: Option<PathBuf>,
}

pub(in crate::subc) struct Service {
    edge: Arc<Edge>,
    connection_file: PathBuf,
    jobs: JoinSet<Option<Event>>,
    authorizer: authorization::Authorizer,
    scopes: scope_lifetime::Tracker,
    pub(in crate::subc) bash: Running,
}

pub(in crate::subc) struct Event(Progress);

enum Progress {
    Folder(Call, PathBuf, ToolCallRequest),
    Preflight(Ready, Preflight),
    Authorized(Ready, authorization::Decision),
    Consent(authorization::Completion),
    Ended(ScopeStamp),
}

struct Call {
    frame: Frame,
    record: Record,
    binding: Arc<Binding>,
    scope: ScopeStamp,
    carrier: String,
    cancel: CancellationToken,
    lifetime: Lifetime,
}

struct Ready {
    call: Call,
    tool: Tool,
    root: ProjectRootId,
    key: ledger::Key,
}

enum Tool {
    File(FileTool),
    Shell(Shell),
}

impl Service {
    #[allow(clippy::too_many_arguments)]
    pub(in crate::subc) fn new(
        control: Arc<readiness::DaemonControl>,
        writer: WriterSender,
        metrics: Arc<DispatchPathMetrics>,
        app: Arc<App>,
        storage_dir: &Path,
        connection_file: &Path,
        executor: Arc<Executor>,
        dispatch: DispatchFn,
        max_body: usize,
    ) -> Self {
        Self {
            edge: Arc::new(Edge {
                writer,
                metrics,
                app,
                database: storage_dir.join("aft.db"),
                executor,
                dispatch,
                max_body,
                #[cfg(all(test, target_os = "linux"))]
                test_launcher: remote_bash::TEST_LAUNCHER.try_with(Clone::clone).ok(),
            }),
            connection_file: connection_file.to_path_buf(),
            jobs: JoinSet::new(),
            authorizer: authorization::Authorizer::new(None, connection_file.to_path_buf()),
            scopes: scope_lifetime::Tracker::new(control),
            bash: Running::default(),
        }
    }

    pub(in crate::subc) fn enroll(&mut self, identity: &ManagementIdentity) {
        if let ManagementAdmission::RemoteTools(_) = &identity.admission {
            if let Some(stamp) = identity
                .scope
                .as_ref()
                .filter(|stamp| admit(Some(stamp)).is_ok())
            {
                self.scopes.enroll(stamp);
            }
        }
    }

    pub(in crate::subc) fn request(
        &mut self,
        frame: Frame,
        body: Option<Value>,
        identity: &ManagementIdentity,
        binding: &Arc<Binding>,
    ) {
        let edge = self.edge.clone();
        if let Some(body) = body
            .as_ref()
            .filter(|body| body["name"] == call_yield::TOOL_YIELD)
        {
            self.yield_shell(frame, body, binding);
            return;
        }
        let operation = body
            .as_ref()
            .and_then(|body| body.get("op").or_else(|| body.get("name")))
            .and_then(Value::as_str)
            .and_then(|op| match op {
                "late_results" => Some("late_results"),
                "late_results.ack" => Some("late_results.ack"),
                _ => None,
            });
        if let Some(op) = operation {
            let Some(scope) = identity
                .scope
                .as_ref()
                .filter(|scope| admit(Some(scope)).is_ok())
            else {
                self.jobs.spawn(async move {
                    edge.error(&frame, admission_refusal()).await;
                    None
                });
                return;
            };
            let custodian = carrier(&principal_label(&identity.principal), scope);
            let mut body = body.expect("a custody operation has a body");
            let arguments = if body.get("op").is_some() {
                body.as_object_mut()
                    .expect("custody envelope is an object")
                    .remove("op");
                body
            } else {
                body.get_mut("arguments")
                    .map(Value::take)
                    .unwrap_or_else(|| json!({}))
            };
            tool_provider::project_free_custody(
                &edge.writer,
                &frame,
                &edge.metrics,
                edge.app.clone(),
                edge.database.clone(),
                op,
                arguments,
                custodian,
                true,
                binding.held_update,
            );
            return;
        }
        let Some(body) = body.filter(|body| {
            body.is_object()
                && (body["op"] == "tool.call"
                    || (body.get("method").is_none()
                        && body.get("name").is_some_and(Value::is_string)
                        && route_request_is_tool_call(body)))
        }) else {
            self.jobs.spawn(async move {
                edge.error(&frame, tool_unavailable()).await;
                None
            });
            return;
        };
        let scope = identity.scope.clone();
        let lifetime = scope
            .as_ref()
            .filter(|stamp| admit(Some(stamp)).is_ok())
            .map(|stamp| self.scopes.lifetime(stamp))
            .unwrap_or_default();
        let principal = principal_label(&identity.principal);
        let binding = binding.clone();
        let cancel = identity.shells_cancel.0.clone();
        let lookup = enumerate(
            #[cfg(test)]
            TEST_REGISTRY.try_with(Arc::clone).ok(),
            self.connection_file.clone(),
        );
        let capacity = self.jobs.len() < MAX_PENDING_LOOKUPS;
        let received_at = ledger::now_ms();
        let tool = body.get("name").and_then(Value::as_str).map(str::to_owned);
        // Start the insert at ingress so a connection teardown that drops the
        // response job cannot erase an already received call from the audit.
        let insert = {
            let edge = edge.clone();
            let scope = scope.clone();
            tokio::task::spawn_blocking(move || {
                Record::open(
                    &edge.app,
                    &edge.database,
                    received_at,
                    scope.as_ref(),
                    tool.as_deref(),
                )
            })
        };
        self.jobs.spawn(async move {
            let _task = ResponseTaskGuard::new(&edge.metrics);
            let record = match insert.await.unwrap_or_else(|error| {
                log::warn!("remote tool audit insert failed: {error}");
                Err(admission_refusal())
            }) {
                Ok(record) => record,
                Err(error) => {
                    edge.error(&frame, error).await;
                    return None;
                }
            };
            let folder = async {
                let (project, _) = admit(scope.as_ref())?;
                if !capacity || cancel.is_cancelled() {
                    return Err(admission_refusal());
                }
                if lifetime.ended() {
                    return Err(authorization::denied());
                }
                match tokio::time::timeout(LOOKUP_TIMEOUT, lookup).await {
                    Ok(Ok(entries)) => registered_root(project, &entries),
                    _ => Err(admission_refusal()),
                }
            }
            .await;
            let mode = body
                .get("name")
                .and_then(Value::as_str)
                .and_then(dispatch::requested_mode)
                .map(|mode| mode.as_str());
            let folder = {
                let record = record.clone();
                blocking(move || {
                    record.resolve(folder.as_ref().ok().map(PathBuf::as_path), mode)?;
                    folder
                })
                .await
            };
            let prepared = folder.and_then(|folder| {
                serde_json::from_value::<ToolCallRequest>(body)
                    .map(|request| (folder, request))
                    .map_err(|error| errors::invalid_request("arguments", error.to_string()))
            });
            match prepared {
                Ok((folder, request)) => {
                    let scope = scope.expect("folder lookup admitted the scope");
                    let carrier = carrier(&principal, &scope);
                    Some(Event(Progress::Folder(
                        Call {
                            frame,
                            record,
                            binding,
                            scope,
                            carrier,
                            cancel,
                            lifetime,
                        },
                        folder,
                        request,
                    )))
                }
                Err(error) => {
                    edge.refuse(&frame, record, None, error).await;
                    None
                }
            }
        });
    }

    pub(in crate::subc) async fn next(&mut self) -> Event {
        loop {
            tokio::select! {
                biased;
                stamp = self.scopes.next() => return Event(Progress::Ended(stamp)),
                completion = self.authorizer.next() => return Event(Progress::Consent(completion)),
                Some(job) = self.jobs.join_next() => match job {
                    Ok(Some(event)) => return event,
                    Ok(None) => {},
                    Err(error) => log::warn!("remote tool call task failed: {error}"),
                },
            }
        }
    }

    pub(in crate::subc) fn advance(
        &mut self,
        Event(event): Event,
        root_channels: &HashMap<ProjectRootId, HashSet<RouteChannel>>,
        routes: &HashMap<RouteChannel, RouteIdentity>,
    ) {
        let edge = self.edge.clone();
        match event {
            Progress::Folder(call, folder, request) => {
                let root = locally_bound_root(&folder, root_channels, routes).cloned();
                self.jobs
                    .spawn(async move { edge.preflight(call, folder, request, root).await });
            }
            Progress::Preflight(ready, consent) => {
                let live = locally_bound_root(&consent.folder, root_channels, routes)
                    .map(|_| ())
                    .and_then(|()| {
                        if ready.call.lifetime.ended() {
                            Err(authorization::denied())
                        } else {
                            Ok(())
                        }
                    });
                self.authorizer.set_db(ready.call.record.db.clone());
                let decision = if live.is_ok() && !ready.call.cancel.is_cancelled() {
                    Some(
                        self.authorizer
                            .request_with_lifetime(consent, ready.call.lifetime.can_ask()),
                    )
                } else {
                    None
                };
                self.jobs.spawn(async move {
                    let authorization = match decision {
                        Some(decision) => tokio::select! {
                            biased;
                            _ = ready.call.cancel.cancelled() => Err(admission_refusal()),
                            result = decision => result.unwrap_or_else(|_| Err(admission_refusal())),
                        },
                        None => live.and(Err(admission_refusal())),
                    };
                    Some(Event(Progress::Authorized(ready, authorization)))
                });
            }
            Progress::Consent(completion) => {
                self.authorizer.finish(completion, root_channels, routes)
            }
            Progress::Ended(stamp) => {
                self.scopes.finish_confirmed_end(&stamp);
                self.authorizer
                    .end_scope(scope_lifetime::pairing(&stamp), &stamp);
            }
            Progress::Authorized(ready, decision) => {
                let decision = decision.and_then(|(consent, authorization)| {
                    locally_bound_root(&consent.folder, root_channels, routes)?;
                    Ok(authorization)
                });
                let bash = self.bash.clone();
                self.jobs.spawn(async move {
                    edge.execute(ready, decision, bash).await;
                    None
                });
            }
        }
    }
}

enum Admission {
    New(ledger::Key, Tool, Preflight),
    Replay(RecordedFrame),
}

impl Edge {
    async fn preflight(
        &self,
        call: Call,
        folder: PathBuf,
        request: ToolCallRequest,
        root: Result<ProjectRootId, ErrorBody>,
    ) -> Option<Event> {
        let root = match root {
            Ok(root) => root,
            Err(error) => {
                self.refuse(&call.frame, call.record, None, error).await;
                return None;
            }
        };
        let result = {
            let (ctx, binding, scope, carrier, record) = (
                self.executor.actor_context(&root),
                call.binding.clone(),
                call.scope.clone(),
                call.carrier.clone(),
                call.record.clone(),
            );
            let trackable = call.lifetime.can_ask();
            blocking(move || {
                let ctx = ctx.ok_or_else(not_working)?;
                let pin = remote_pins::parse(&request)?;
                let preset = CatalogPreset::parse("preset", request.preset.as_deref())?;
                if dispatch::operation(&request.name).is_none() || !remote_pins::preset_serves(preset, &request.name) {
                    return Err(tool_unavailable());
                }
                binding.check_transport(preset)?;
                remote_pins::validate(&request, &pin)?;
                let call_key = request.call_key.clone().filter(|key| !key.is_empty()).ok_or_else(|| {
                    errors::invalid_request("call_key", "remote tool calls require a call key")
                })?;
                let key = ledger::Key { carrier, call_key };
                let digest = composition_digest(&json!({"name":request.name, "schema_pin":request.schema_pin, "arguments":request.arguments}))
                    .map_err(|error| errors::invalid_request("arguments", error.to_string()))?;
                let (tool, preflight) = if dispatch::operation(&request.name).is_some_and(|(op, _)| op == "bash") {
                    if !trackable {
                        return Err(admission_refusal());
                    }
                    let (shell, preflight) = Shell::prepare(request, scope, folder, binding.session.clone(), digest.clone(), &ctx)?;
                    (Tool::Shell(shell), preflight)
                } else {
                    let (file, preflight) = FileTool::prepare(request, scope, folder, binding.session.clone(), digest.clone(), &ctx)?;
                    (Tool::File(file), preflight)
                };
                match record.claim(&key, &digest, &preflight.scope)? {
                    ledger::Admission::New => Ok(Admission::New(key, tool, preflight)),
                    ledger::Admission::Repeat(row) => row.frame.map(Admission::Replay).ok_or_else(|| {
                        ErrorBody::new("call_in_progress", "This remote call is already in progress.")
                    }),
                    ledger::Admission::Conflict => Err(errors::invalid_request("call_key", "key already names different content or scope")),
                }
            }).await
        };
        match result {
            Ok(Admission::New(key, tool, consent)) => Some(Event(Progress::Preflight(
                Ready {
                    call,
                    tool,
                    root,
                    key,
                },
                consent,
            ))),
            Ok(Admission::Replay(reply)) => {
                self.finish(&call.frame, call.record, None, reply, "replay", None)
                    .await;
                None
            }
            Err(error) => {
                self.refuse(&call.frame, call.record, None, error).await;
                None
            }
        }
    }

    async fn execute(
        &self,
        ready: Ready,
        authorization: Result<authorization::Authorization, ErrorBody>,
        bash: Running,
    ) {
        use authorization::Authorization;
        let Ready {
            call,
            tool,
            root,
            key,
        } = ready;
        let (decision, grant) = match authorization {
            Ok(Authorization::Once) => ("allow_once", None),
            Ok(Authorization::Saved(id)) => ("saved_grant", Some(id)),
            Ok(Authorization::WorkerRun) => ("worker_run", None),
            Err(error) => {
                self.refuse(&call.frame, call.record, Some(key), error)
                    .await;
                return;
            }
        };
        let tool = match tool {
            Tool::File(tool) => tool,
            Tool::Shell(shell) => {
                self.execute_shell(call, root, key, shell, decision, grant, bash)
                    .await;
                return;
            }
        };
        let dispatch = self.dispatch;
        let record = call.record.clone();
        let dispatch_key = key.clone();
        let cancel = call.cancel.clone();
        let lifetime = call.lifetime.clone();
        let (result_tx, result_rx) = oneshot::channel();
        let request_id = format!(
            "remote-file-{}-{}",
            call.frame.header.channel, call.frame.header.corr
        );
        let rx = self.executor.submit_async(
            root,
            tool.lane(),
            request_id.clone(),
            Box::new(move |ctx| {
                let result = (|| {
                    if lifetime.ended() {
                        return Err(authorization::denied());
                    }
                    if cancel.is_cancelled() {
                        return Err(admission_refusal());
                    }
                    record.begin(&dispatch_key, decision)?;
                    tool.run(ctx, dispatch)
                })();
                let _ = result_tx.send(result);
                Response::success(request_id, json!({}))
            }),
        );
        let _response = await_executor_response(rx, "remote-file".into()).await;
        let result = result_rx.await.unwrap_or_else(|_| Err(not_working()));
        let reply = self.tool_reply(&call, result);
        self.finish(&call.frame, call.record, Some(key), reply, decision, None)
            .await;
    }

    fn tool_reply(&self, call: &Call, result: Result<ToolCallResult, ErrorBody>) -> RecordedFrame {
        match result {
            Ok(result) => {
                let build = || {
                    let frame = &call.frame;
                    let mut policy: wire::ToolResponsePolicy = BindTrust::Untrusted.into();
                    policy.tool_result_body = call.binding.tool_result_body;
                    let mut outgoing = wire::build_tool_response_frame_with_limit(
                        frame.header.ver,
                        route_key(frame.header.channel, frame.header.epoch),
                        frame.header.corr,
                        frame.header.flags,
                        &result,
                        policy,
                        self.max_body,
                    )?;
                    outgoing.project_tool_result()?;
                    Ok::<_, SubcError>(RecordedFrame {
                        ty: outgoing.frame().header.ty,
                        body: outgoing.frame().body.clone(),
                    })
                };
                build().unwrap_or_else(|error| {
                    log::warn!("remote tool result serialization failed: {error}");
                    error_reply(ErrorBody::new(
                        "internal_error",
                        "The remote tool reply could not be encoded.",
                    ))
                })
            }
            Err(error) => error_reply(error),
        }
    }

    async fn refuse(
        &self,
        frame: &Frame,
        record: Record,
        key: Option<ledger::Key>,
        error: ErrorBody,
    ) {
        let decision = if error.code == authorization::denied().code {
            "denied"
        } else {
            "refused"
        };
        self.finish(
            frame,
            record,
            key,
            error_reply(error),
            decision,
            Some("refused"),
        )
        .await;
    }

    async fn finish(
        &self,
        frame: &Frame,
        record: Record,
        key: Option<ledger::Key>,
        reply: RecordedFrame,
        decision: &'static str,
        reason: Option<&'static str>,
    ) {
        self.settle(frame, record, key, reply, decision, reason, false)
            .await;
    }

    #[allow(clippy::too_many_arguments)]
    async fn settle(
        &self,
        frame: &Frame,
        record: Record,
        key: Option<ledger::Key>,
        reply: RecordedFrame,
        decision: &'static str,
        reason: Option<&'static str>,
        held: bool,
    ) {
        let settled = blocking(move || {
            record.finish(key.as_ref(), &reply, decision, reason)?;
            Ok(reply)
        })
        .await;
        match settled {
            Ok(_) if held => {}
            Ok(reply) => {
                if let Ok(response) = Frame::build_with_version(
                    frame.header.ver,
                    reply.ty,
                    frame.header.flags,
                    frame.header.channel,
                    frame.header.epoch,
                    frame.header.corr,
                    reply.body,
                ) {
                    let _ = send_reliable_writer_frame(
                        &self.writer,
                        &self.metrics,
                        response,
                        "remote tool reply",
                    )
                    .await;
                }
            }
            Err(error) if !held => self.error(frame, error).await,
            Err(_) => {}
        }
    }

    async fn error(&self, request: &Frame, error: ErrorBody) {
        let _ = send_provider_error(&self.writer, &self.metrics, request, error).await;
    }
}

fn error_reply(error: ErrorBody) -> RecordedFrame {
    RecordedFrame {
        ty: FrameType::Error,
        body: serde_json::to_vec(&error).expect("error body is JSON"),
    }
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, ErrorBody> + Send + 'static,
) -> Result<T, ErrorBody> {
    tokio::task::spawn_blocking(f)
        .await
        .unwrap_or_else(|error| {
            log::warn!("remote tool preparation or storage failed: {error}");
            Err(admission_refusal())
        })
}
