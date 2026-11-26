use super::*;
use serde_json::json;

pub struct IdleLspFixture {
    pub(crate) ctx: std::sync::Arc<crate::context::AppContext>,
    pub(crate) config: Config,
    pub(crate) busy: PathBuf,
    pub(crate) idle: PathBuf,
    _tmp: tempfile::TempDir,
}

impl IdleLspFixture {
    pub fn with_server_binary(server_binary: impl FnOnce() -> PathBuf) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let config = Config {
            project_root: Some(tmp.path().to_path_buf()),
            lsp_idle_minutes: 5,
            ..Config::default()
        };
        let ctx = std::sync::Arc::new(crate::context::AppContext::new(
            crate::context::default_language_provider_factory(),
            config.clone(),
        ));
        ctx.lsp()
            .override_binary(ServerKind::TypeScript(None), server_binary());
        ctx.lsp().set_extra_env("AFT_FAKE_LSP_IGNORE_SHUTDOWN", "1");
        ctx.lsp().set_extra_env("AFT_FAKE_LSP_REPORT_SHUTDOWN", "1");
        let busy = tmp.path().join("busy/index.ts");
        let idle = tmp.path().join("idle/index.ts");
        for (index, file) in [&busy, &idle].into_iter().enumerate() {
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file.parent().unwrap().join("tsconfig.json"), "{}").unwrap();
            // Distinct SDKs retain two clients even with a shared repository root.
            let sdk = file.parent().unwrap().join("node_modules/typescript");
            std::fs::create_dir_all(&sdk).unwrap();
            std::fs::write(
                sdk.join("package.json"),
                format!(r#"{{"version":"5.9.{index}"}}"#),
            )
            .unwrap();
            std::fs::write(file, "export const n = 1;\n").unwrap();
            assert_eq!(
                settled_lsp(&mut ctx.lsp(), |manager| {
                    manager.ensure_server_for_file(file, &config)
                })
                .len(),
                1
            );
            ctx.lsp()
                .client_for_file_mut(file, &config)
                .unwrap()
                .set_last_used_at_for_test(Instant::now() - Duration::from_secs(301));
        }
        let fixture = Self {
            ctx,
            config,
            busy,
            idle,
            _tmp: tmp,
        };
        fixture.hover(&fixture.busy);
        fixture
    }

    pub fn hover(&self, file: &Path) {
        self.ctx.note_request();
        let hover = self
            .ctx
            .lsp()
            .client_for_file_mut(file, &self.config)
            .unwrap()
            .send_request::<lsp_types::request::HoverRequest>(idle_hover_params(file))
            .unwrap();
        assert!(hover.is_some());
    }

    pub fn pid(&self, file: &Path) -> Option<u32> {
        self.ctx
            .lsp()
            .client_for_file(file, &self.config)
            .map(LspClient::child_pid)
    }

    pub fn assert_only_busy(&self) {
        assert!(
            self.pid(&self.busy).is_some(),
            "recently requested server must survive"
        );
        assert!(
            self.pid(&self.idle).is_none(),
            "idle package must be detached despite a busy root"
        );
    }
}

pub(super) fn idle_hover_params(file: &Path) -> lsp_types::HoverParams {
    serde_json::from_value(json!({
        "textDocument": { "uri": url::Url::from_file_path(file).unwrap().as_str() },
        "position": { "line": 0, "character": 0 }
    }))
    .unwrap()
}

pub fn enqueue_maintenance_backlog_for_test(manager: &mut LspManager, root: &Path) -> impl Drop {
    let event = LspEvent::Notification {
        server_kind: ServerKind::Rust,
        root: root.to_path_buf(),
        method: "custom/pending".into(),
        params: None,
    };
    // One maintenance batch fits both queues, but a reader can also
    // leave a final exit in its priority slot when ingress is full.
    for _ in 0..EVENT_CAP {
        manager.enqueue_event_for_test(event.clone());
        manager
            .ready_startup_events
            .push_back(StartupEventDelivery {
                event: event.clone().into(),
                published: None,
            });
    }
    let instance = ServerInstanceId::next();
    let root = root.to_path_buf();
    let key = ServerKey {
        kind: ServerKind::Rust,
        root: root.clone(),
    };
    let lease = manager.event_tx.register(instance, key);
    manager
        .event_tx
        .send(ReceivedLspEvent {
            event: LspEvent::ServerExited {
                server_kind: ServerKind::Rust,
                root,
                pid: 0,
                reason: crate::lsp::client::ServerExitReason::Eof,
            },
            instance: Some(instance),
            receive_order: None,
        })
        .unwrap();
    assert!(manager.event_tx.exit_pending_for_test(instance));
    lease
}
/// Inject an unversioned push received after the file's last document sync.
/// Return the open document's version for comparison with the diagnostic report.
pub fn publish_after_document_sync_for_test(
    manager: &mut LspManager,
    key: &ServerKey,
    path: &Path,
    params: &serde_json::Value,
) -> Option<i32> {
    let document = manager.documents.get(key)?.entry(path)?;
    let version = document.version;
    let received_after_sync = document.sync_order? + 1;
    manager.handle_publish_diagnostics_ordered(
        key.kind.clone(),
        key.root.clone(),
        params,
        Some(received_after_sync),
    );
    Some(version)
}

pub(super) fn lifecycle_fake_client_with_env<'a>(
    manager: &LspManager,
    key: &ServerKey,
    binary: &Path,
    env: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> LspClient {
    let env = [("AFT_FAKE_LSP_DISABLE_PUSH", "1")]
        .into_iter()
        .chain(env)
        .map(|(key, value)| (key.into(), value.into()))
        .collect();
    let mut client = LspClient::spawn_with_reclaim_root(
        key.kind.clone(),
        key.root.clone(),
        binary,
        &[],
        &env,
        LspEventSender::Managed {
            sender: manager.event_tx.clone(),
            instance: ServerInstanceId::next(),
        },
        manager.child_registry.clone(),
        None,
        false,
    )
    .unwrap();
    client.initialize(&key.root, None).unwrap();
    client
}

#[cfg(unix)]
pub const FLYCHECK_PUBLISH_SETTLE_FOR_TEST: Duration = FLYCHECK_PUBLISH_SETTLE;

#[cfg(any(test, unix))]
pub(super) fn fill_progress_queue(manager: &LspManager, key: &ServerKey, mut order: u64) -> u64 {
    while manager.event_rx.len() < EVENT_CAP {
        send_work_done(manager, key, order, "ignored", "report");
        order += 1;
    }
    assert_eq!(manager.event_rx.len(), EVENT_CAP);
    order
}

#[cfg(any(test, unix))]
pub(super) fn send_work_done(
    manager: &LspManager,
    key: &ServerKey,
    order: u64,
    token: &str,
    kind: &str,
) {
    let event = if kind == "create" {
        LspEvent::ServerRequest {
            server_kind: key.kind.clone(),
            root: key.root.clone(),
            id: crate::lsp::jsonrpc::RequestId::Int(1),
            method: "window/workDoneProgress/create".into(),
            params: Some(json!({"token": token})),
        }
    } else {
        LspEvent::Notification {
            server_kind: key.kind.clone(),
            root: key.root.clone(),
            method: "$/progress".into(),
            params: Some(json!({"token": token, "value": {"kind": kind, "title": "cargo check"}})),
        }
    };
    send_progress_fixture_event(manager, key, order, event);
}

#[cfg(any(test, unix))]
pub(super) fn send_progress_fixture_event(
    manager: &LspManager,
    key: &ServerKey,
    order: u64,
    event: LspEvent,
) {
    manager
        .event_tx
        .send(ReceivedLspEvent {
            event,
            instance: Some(manager.clients[key].instance),
            receive_order: Some(order),
        })
        .unwrap();
}

#[cfg(unix)]
pub fn drain_flycheck_end_for_maintenance_test(
    manager: &mut LspManager,
    root: &Path,
    storage: &Path,
    binary: &Path,
) -> (ServerKey, Instant) {
    use crate::lsp::completed_rust_check::{CompletedRustCheck, Runtime};

    let key = ServerKey {
        kind: ServerKind::Rust,
        root: canonicalize_for_lsp(root).unwrap(),
    };
    let mut client = lifecycle_fake_client_with_env(
        manager,
        &key,
        binary,
        [("AFT_FAKE_LSP_SERVER_STATUS", "disabled")],
    );
    client.set_rust_analyzer_quiescent(true);
    let cache = CompletedRustCheck::new(
        &key.root,
        &key.root,
        storage,
        Runtime {
            binary: binary.to_path_buf(),
            args: Vec::new(),
            env: HashMap::new(),
            options: None,
            launch_env: None,
            #[cfg(test)]
            validation_gate: None,
        },
    )
    .unwrap();
    client.completed_rust_check = Some(cache);
    manager.clients.insert(key.clone(), client);
    finish_flycheck_for_maintenance_test(manager, &key, 1);
    let settle_at = manager.clients[&key]
        .rust_publish_settle_deadline()
        .unwrap();
    (key, settle_at)
}

#[cfg(unix)]
pub fn finish_flycheck_for_maintenance_test(
    manager: &mut LspManager,
    key: &ServerKey,
    begin_order: u64,
) {
    // Stamp BEGIN three seconds in the future, so the check counts as
    // starting after the Cargo inputs this test just wrote.
    manager.clients[key].receive_flycheck_begin_for_test(
        begin_order,
        std::time::SystemTime::now() + Duration::from_secs(3),
    );
    let token = "rust-analyzer/flycheck/0";
    send_work_done(manager, key, begin_order, token, "begin");
    send_work_done(manager, key, begin_order + 1, token, "end");
    manager.drain_events();
}

#[cfg(unix)]
pub fn overflow_flycheck_inbox_for_maintenance_test(
    manager: &mut LspManager,
    key: &ServerKey,
    file: &Path,
) -> u64 {
    send_progress_fixture_event(
        manager,
        key,
        3,
        LspEvent::Notification {
            server_kind: key.kind.clone(),
            root: key.root.clone(),
            method: "textDocument/publishDiagnostics".into(),
            params: Some(json!({
                "uri": uri_for_path(file).unwrap(),
                "diagnostics": [{"range": {"start": {"line": 0, "character": 0},
                    "end": {"line": 0, "character": 1}}, "severity": 1, "source": "rustc",
                    "message": "compiler report from the invalidated check"}]
            })),
        },
    );
    manager.drain_events();
    assert!(manager.clients[key]
        .completed_rust_check
        .as_ref()
        .unwrap()
        .reports
        .contains_key(file));
    let dropped = fill_progress_queue(manager, key, 4);
    send_work_done(manager, key, dropped, "discarded", "report");
    manager.drain_events();
    dropped + 1
}
