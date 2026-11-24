use super::*;
use subc_protocol::session::{HealthReport, HealthStatus, MODULE_CONTROL_OP_HEALTH_CHECK};

pub(in crate::subc) struct LegacySettings {
    _watch: Option<crate::config_live::ConfigFileWatch>,
}

impl LegacySettings {
    pub(in crate::subc) fn watch(path: Option<&Path>, executor: &Arc<Executor>) -> Self {
        let _watch = path.map(|path| {
            let executor = Arc::downgrade(executor);
            crate::config_live::start_process_user_config_watch(path.to_path_buf(), move || {
                executor
                    .upgrade()
                    .map(|executor| executor.actor_contexts())
                    .unwrap_or_default()
            })
        });
        Self { _watch }
    }
}

pub(in crate::subc) struct Hello {
    pub(in crate::subc) ack: Option<ModuleHelloAckBody>,
    pub(in crate::subc) provides: Vec<subc_protocol::manifest::ProviderRole>,
    pub(in crate::subc) rich_ui: rich_ui::session::ProofCapabilities,
    pub(in crate::subc) at: tokio::time::Instant,
}

impl Hello {
    pub(in crate::subc) fn gated(&self) -> bool {
        self.ack.as_ref().is_some_and(|ack| {
            ack.subc_capabilities
                .iter()
                .any(|cap| cap == CONFIG_GATE_CAPABILITY)
        })
    }
}

pub(in crate::subc) async fn negotiate<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    read: &mut R,
    write: &mut W,
    user_path: Option<&Path>,
    manage_settings: bool,
    using_registration: bool,
) -> Result<Hello, SubcError> {
    #[cfg(all(feature = "test-hooks", debug_assertions))]
    if let Some(marker) = std::env::var_os("AFT_TEST_SETTINGS_HELLO_MARKER") {
        let marker = PathBuf::from(marker);
        let _ = tokio::time::timeout(Duration::from_secs(20), async {
            while !marker.is_file() {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await;
    }
    let disabled = if manage_settings {
        Vec::new()
    } else {
        crate::subc_config::catalog_disabled_tools(user_path)
    };
    let launch_nonce = (!using_registration)
        .then(crate::launch_nonce::current)
        .flatten();
    tool_provider::prepare_admission();
    let mut manifest = build_manifest_without(&disabled);
    manifest::declare_provenance(&mut manifest, crate::launch_nonce::provenance());
    let rich_ui = rich_ui::session::ProofCapabilities::from_manifest(&manifest);
    let provides = manifest.provides.clone();
    let at = tokio::time::Instant::now();
    let mut ops = control_ops().unwrap_or_default();
    if manage_settings {
        ops.push(CONFIG_REVIEW_OP.to_string());
    }
    let hello = ModuleHelloBody {
        manifest,
        protocol_ver: PROTOCOL_VERSION,
        control_ops: Some(ops),
        launch_nonce: launch_nonce.map(|nonce| nonce.value().to_string()),
    };
    let frame = Frame::build(
        FrameType::Hello,
        control_flags(),
        0,
        0,
        HELLO_CORR,
        serde_json::to_vec(&hello).map_err(SubcError::Json)?,
    )
    .map_err(SubcError::FrameBuild)?;
    write_frame(write, &frame)
        .await
        .map_err(SubcError::FrameIo)?;
    let ack = match read_frame(read).await.map_err(SubcError::FrameIo)? {
        None => return Err(SubcError::ClosedBeforeHelloAck),
        Some(frame) => match frame.header.ty {
            FrameType::HelloAck => {
                log::info!("subc attach: registered (HelloAck received)");
                serde_json::from_slice::<ModuleHelloAckBody>(&frame.body).ok()
            }
            FrameType::Error => {
                return Err(SubcError::HelloRejected {
                    body: serde_json::from_slice(&frame.body).ok(),
                })
            }
            ty => return Err(SubcError::UnexpectedFrame { ty }),
        },
    };
    // A malformed acknowledgement cannot establish whether the daemon supports config.gate.
    if manage_settings && ack.is_none() {
        return Err(SubcError::UnexpectedFrame {
            ty: FrameType::HelloAck,
        });
    }
    Ok(Hello {
        ack,
        provides,
        rich_ui,
        at,
    })
}

pub(in crate::subc) async fn wait_for_first_view(
    control: &readiness::DaemonControl,
    ctx: &Arc<AppContext>,
    user_path: Option<&Path>,
    reader: &mut mpsc::Receiver<Result<DecodedFrame, SubcError>>,
    replies: &readiness::PendingControlReplies,
    writer: &WriterSender,
    metrics: &DispatchPathMetrics,
) -> Result<Result<Arc<Session>, ModuleLoopExit>, SubcError> {
    let declaration = declaration(user_path);
    drive_first_view(control, ctx, declaration, reader, replies, writer, metrics).await
}

pub(super) async fn drive_first_view(
    control: &readiness::DaemonControl,
    ctx: &Arc<AppContext>,
    declaration: Option<(ConfigDeclareRequest, Schema)>,
    reader: &mut mpsc::Receiver<Result<DecodedFrame, SubcError>>,
    replies: &readiness::PendingControlReplies,
    writer: &WriterSender,
    metrics: &DispatchPathMetrics,
) -> Result<Result<Arc<Session>, ModuleLoopExit>, SubcError> {
    let declaration = declaration.map(|(request, schema)| (request, Arc::new(schema)));
    let mut reason = if declaration.is_some() {
        "waiting_for_settings"
    } else {
        "settings_declaration_unavailable"
    };
    let mut should_fetch = declaration.is_some();
    let mut declare = should_fetch;
    loop {
        let attempt_fetch = should_fetch;
        let attempt_declare = declare;
        let fetch = async {
            if !attempt_fetch {
                return std::future::pending().await;
            }
            let (input, schema) = declaration
                .as_ref()
                .ok_or("settings_declaration_unavailable")?;
            if attempt_declare {
                let _: ConfigDeclareReply = request(control, input).await?;
            }
            let reply: ConfigGetReply = request(control, &ConfigGetRequest::new()).await?;
            let recovery = reply.pending.as_ref().and_then(|_| reply.active.clone());
            let mut view = view_from_reply(reply)?;
            let candidate = view.clone();
            let apply_ctx = ctx.clone();
            let applied =
                tokio::task::spawn_blocking(move || apply_initial(&apply_ctx, &candidate))
                    .await
                    .unwrap_or(Err("settings_apply_failed"));
            let (tier, disabled) = match applied {
                Ok(applied) => applied,
                Err(reason) => {
                    report(control, &view, Err(reason)).await?;
                    view = recovery
                        .filter(|view| view.status == ConfigStatus::Active)
                        .ok_or(reason)?;
                    let candidate = view.clone();
                    let ctx = ctx.clone();
                    tokio::task::spawn_blocking(move || apply_initial(&ctx, &candidate))
                        .await
                        .unwrap_or(Err("settings_apply_failed"))?
                }
            };
            tokio::time::timeout(
                CONFIG_TIMEOUT,
                control.install_settings_catalog(build_manifest_without(&disabled).provides),
            )
            .await
            .map_err(|_| "settings_catalog_unavailable")?
            .map_err(|_| "settings_catalog_unavailable")?;
            report(control, &view, Ok(())).await?;
            Ok::<_, &'static str>(Arc::new(Session::new(schema.clone(), tier, view)))
        };
        tokio::pin!(fetch);
        let mut changed = false;
        loop {
            tokio::select! {
                outcome = &mut fetch => match outcome {
                    Ok(session) => {
                        if changed { session.hint.notify_one(); }
                        return Ok(Ok(session));
                    }
                    Err(error) => { log::warn!("settings startup waiting: {error}"); reason = error; break; }
                },
                incoming = reader.recv() => {
                    let Some(incoming) = incoming else { return Ok(Err(ModuleLoopExit::ConnectionLost)); };
                    let frame = incoming?.frame;
                    if frame.header.channel == 0 && frame.header.epoch == 0 && frame.header.ty == FrameType::Goodbye {
                        return Ok(Err(ModuleLoopExit::Graceful));
                    }
                    if frame.header.channel == 0 && matches!(frame.header.ty, FrameType::Response | FrameType::Error) {
                        replies.resolve(frame);
                    } else if declaration.is_some() && is_changed(&frame) {
                        changed = true;
                        if !should_fetch { break; }
                    } else if let Some(reply) = pre_ready_reply(&frame, declaration.as_ref().map(|(_, schema)| schema), reason).await? {
                        send_frame(writer, metrics, reply).await?;
                    }
                }
            }
        }
        declare = false;
        should_fetch = changed;
        if declaration.is_none() {
            reason = "settings_declaration_unavailable";
        }
    }
}

fn apply_initial(
    ctx: &AppContext,
    view: &ConfigView,
) -> Result<(ConfigTier, Vec<String>), &'static str> {
    let (tier, mut config) = resolve_user(view)?;
    config.storage_dir = ctx.config_unpinned().storage_dir.clone();
    crate::root_cache::prepare_process_cache_root(&mut config);
    if config.storage_lease.is_none() || config.cache_unavailable.is_some() {
        return Err("settings_storage_unavailable");
    }
    aft_store::sync::set_macos_full_sync(config.durability.macos_full_sync);
    crate::memory::profile::publish(config.memory);
    let disabled = config.disabled_tools.clone();
    ctx.set_config(config);
    Ok((tier, disabled))
}

pub(super) async fn pre_ready_reply(
    frame: &Frame,
    schema: Option<&Arc<Schema>>,
    reason: &str,
) -> Result<Option<Frame>, SubcError> {
    if let Some(schema) = schema {
        if let Some(reply) = review::response(schema, frame).await? {
            return Ok(Some(reply));
        }
    }
    let (kind, body) = match frame.header.ty {
        FrameType::Ping if frame.header.channel == 0 => (FrameType::Pong, Vec::new()),
        FrameType::Request => {
            let health_check = frame.header.channel == 0
                && serde_json::from_slice::<Value>(&frame.body)
                    .is_ok_and(|body| body["op"] == MODULE_CONTROL_OP_HEALTH_CHECK);
            if health_check {
                let report = HealthReport {
                    status: HealthStatus::Degraded,
                    detail: Some(WAITING_HEALTH.into()),
                    metrics: Some(json!({"settings": {"ready": false, "reason": reason}})),
                };
                (
                    FrameType::Response,
                    serde_json::to_vec(&ModuleControlResponse::from(report))
                        .map_err(SubcError::Json)?,
                )
            } else {
                (
                    FrameType::Error,
                    serde_json::to_vec(&ErrorBody::new("module_warming", "module is warming"))
                        .map_err(SubcError::Json)?,
                )
            }
        }
        _ => return Ok(None),
    };
    Frame::build_with_version(
        frame.header.ver,
        kind,
        frame.header.flags,
        frame.header.channel,
        frame.header.epoch,
        frame.header.corr,
        body,
    )
    .map(Some)
    .map_err(SubcError::FrameBuild)
}

pub(in crate::subc) fn initialize_storage(app: Arc<App>, storage_dir: &Path) {
    crate::context::start_runner_harness_copy(app, storage_dir.join("aft.db"));
    crate::bash_background::note_storage_root(storage_dir);
    crate::page_collect::snapshot::SnapshotStore::for_storage(storage_dir);
}

pub(in crate::subc) fn initialize_task_cleanup(ctx: &AppContext, storage_dir: &Path) {
    ctx.bash_background().configure_task_scope_runtime(
        ctx.app().subc_connection_file().is_some(),
        Duration::from_secs(ctx.config().bash.linux_scope_max_runtime_secs),
    );
    ctx.bash_background().start_task_scope_cleanup(storage_dir);
}

pub(in crate::subc) fn legacy_catalog(control: &readiness::DaemonControl, path: Option<&Path>) {
    // The ready flip publishes these filtered roles in the same update.
    // Daemons without catalog.update ignore HELLO's not-ready flag and
    // keep its unfiltered catalog; dispatch still checks disabled_tools.
    let disabled = crate::subc_config::catalog_disabled_tools(path);
    control.set_catalog(build_manifest_without(&disabled).provides);
}
