use super::*;
use crate::lsp::client::ServerExitReason;
use crate::lsp::registry::builtin_servers;
use serde_json::json;

impl LspManager {
    pub(crate) fn enqueue_maintenance_backlog_for_test(&mut self, root: &Path) -> impl Drop {
        let event = LspEvent::Notification {
            server_kind: ServerKind::Rust,
            root: root.to_path_buf(),
            method: "custom/pending".into(),
            params: None,
        };
        // One maintenance batch fits both queues, but a reader can also
        // leave a final exit in its priority slot when ingress is full.
        for _ in 0..EVENT_CAP {
            self.enqueue_event_for_test(event.clone());
            self.ready_startup_events.push_back(StartupEventDelivery {
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
        let lease = self.event_tx.register(instance, key);
        self.event_tx
            .send(ReceivedLspEvent {
                event: LspEvent::ServerExited {
                    server_kind: ServerKind::Rust,
                    root,
                    pid: 0,
                    reason: ServerExitReason::Eof,
                },
                instance: Some(instance),
                receive_order: None,
            })
            .unwrap();
        assert!(self.event_tx.exit_pending_for_test(instance));
        lease
    }
}

pub(crate) struct IdleLspFixture {
    pub(crate) ctx: std::sync::Arc<crate::context::AppContext>,
    pub(crate) config: Config,
    pub(crate) busy: PathBuf,
    pub(crate) idle: PathBuf,
    _tmp: tempfile::TempDir,
}

impl IdleLspFixture {
    pub(crate) fn new() -> Self {
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
        ctx.lsp().override_binary(
            ServerKind::TypeScript(None),
            crate::fake_lsp_test_helper::fake_server_binary(),
        );
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

    pub(crate) fn hover(&self, file: &Path) {
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

    pub(crate) fn pid(&self, file: &Path) -> Option<u32> {
        self.ctx
            .lsp()
            .client_for_file(file, &self.config)
            .map(LspClient::child_pid)
    }

    pub(crate) fn assert_only_busy(&self) {
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

fn idle_hover_params(file: &Path) -> lsp_types::HoverParams {
    serde_json::from_value(json!({
        "textDocument": { "uri": url::Url::from_file_path(file).unwrap().as_str() },
        "position": { "line": 0, "character": 0 }
    }))
    .unwrap()
}

#[test]
fn server_capacity_evicts_least_recently_used_client_and_clears_its_diagnostics() {
    let fixture = IdleLspFixture::new();
    let key = fixture
        .ctx
        .lsp()
        .server_key_for_file(&fixture.idle, &fixture.config)
        .unwrap();
    {
        let mut manager = fixture.ctx.lsp();
        seed_instance_state(&mut manager, &key, &fixture.idle);
        manager.make_server_capacity(2, 2).unwrap();
        assert_eq!(manager.server_count(), 1);
        assert!(!manager.documents.contains_key(&key));
        assert!(!manager.server_binaries.contains_key(&key));
        assert!(!manager
            .diagnostics
            .has_report_for_server_file(&key, &fixture.idle));
    }
    fixture.assert_only_busy();
}

#[test]
fn server_capacity_counts_reservations_refuses_without_caching_and_retries() {
    let tmp = tempfile::tempdir().unwrap();
    let mut manager = LspManager::new();
    manager.override_binary(
        ServerKind::TypeScript(None),
        crate::fake_lsp_test_helper::fake_server_binary(),
    );
    let config = Config {
        lsp_max_servers: 2,
        ..Config::default()
    };
    let def = builtin_servers()
        .into_iter()
        .find(|def| matches!(def.kind, ServerKind::TypeScript(_)))
        .unwrap();
    let mut prepared = Vec::new();
    let mut keys = Vec::new();
    for index in 0..23 {
        let root = tmp.path().join(format!("package{index}"));
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("index.ts");
        let key = ServerKey {
            kind: def.kind.clone(),
            root,
        };
        let start = manager.begin_file_server_start(&def, &key, &file, &config);
        if index < 2 {
            let FileServerStart::Spawn(spawn) = start else {
                panic!("start should reserve capacity")
            };
            prepared.push(spawn);
            keys.push(key);
        } else {
            let FileServerStart::Failed(ServerAttemptResult::SpawnFailed { reason, .. }) = start
            else {
                panic!("capacity must refuse start")
            };
            assert!(reason.contains("language server limit reached (lsp.max_servers=2); all running servers are starting or have pending requests"));
            assert!(manager.failure_to_replay(&key).is_none());
        }
        assert!(manager.server_count() <= 2);
    }
    let error = manager.make_server_capacity(2, 2).unwrap_err();
    assert_eq!(error.durability, FailureDurability::Capacity);
    let LspError::ServerNotReady(reason) = error.error else {
        panic!("expected capacity reason")
    };
    assert_eq!(reason, "language server limit reached (lsp.max_servers=2); all running servers are starting or have pending requests");
    manager.release_start_reservation(&keys[0]);
    let retry = ServerKey {
        kind: def.kind.clone(),
        root: tmp.path().join("package22"),
    };
    assert!(matches!(
        manager.begin_file_server_start(&def, &retry, &retry.root.join("index.ts"), &config),
        FileServerStart::Spawn(_)
    ));
    manager.starting.clear();
}

fn capacity_manager(
    shared: &Arc<parking_lot::Mutex<ServerCapacity>>,
) -> Arc<parking_lot::Mutex<LspManager>> {
    let manager = Arc::new(parking_lot::Mutex::new(LspManager::new()));
    {
        let mut locked = manager.lock();
        locked.set_server_capacity(Arc::clone(shared));
        locked.enable_background_document_pulls(Arc::downgrade(&manager));
        locked.override_binary(
            ServerKind::TypeScript(None),
            crate::fake_lsp_test_helper::fake_server_binary(),
        );
    }
    manager
}

#[test]
fn server_capacity_parallel_roots_reserve_atomically_and_dropped_roots_release_slots() {
    let tmp = tempfile::tempdir().unwrap();
    let shared = Arc::new(parking_lot::Mutex::new(ServerCapacity::default()));
    let managers: Vec<_> = (0..3).map(|_| capacity_manager(&shared)).collect();
    let barrier = Arc::new(std::sync::Barrier::new(23));
    let def = builtin_servers()
        .into_iter()
        .find(|def| matches!(def.kind, ServerKind::TypeScript(_)))
        .unwrap();
    let mut workers = Vec::new();
    for index in 0..23 {
        let manager = Arc::clone(&managers[index % managers.len()]);
        let barrier = Arc::clone(&barrier);
        let def = def.clone();
        let root = tmp.path().join(format!("root{index}"));
        std::fs::create_dir_all(&root).unwrap();
        workers.push(std::thread::spawn(move || {
            let config = Config {
                lsp_max_servers: 8,
                lsp_user_max_servers: Some(8),
                ..Config::default()
            };
            let key = ServerKey {
                kind: def.kind.clone(),
                root,
            };
            barrier.wait();
            let start = manager.lock().begin_file_server_start(
                &def,
                &key,
                &key.root.join("index.ts"),
                &config,
            );
            matches!(start, FileServerStart::Spawn(_))
        }));
    }
    assert_eq!(
        workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .filter(|started| *started)
            .count(),
        8
    );
    assert_eq!(
        managers
            .iter()
            .map(|manager| manager.lock().server_count())
            .sum::<usize>(),
        8
    );
    assert_eq!(
        shared
            .lock()
            .slots
            .iter()
            .filter(|slot| slot.strong_count() > 0)
            .count(),
        8
    );
    drop(managers);
    assert!(shared
        .lock()
        .slots
        .iter()
        .all(|slot| slot.strong_count() == 0));
    let manager = capacity_manager(&shared);
    assert!(manager.lock().make_server_capacity(1, 8).is_ok());
}

#[test]
fn server_capacity_two_manager_start_race_never_waits_on_the_other_manager() {
    let shared = Arc::new(parking_lot::Mutex::new(ServerCapacity::default()));
    let first = capacity_manager(&shared);
    let second = capacity_manager(&shared);
    let slot = first.lock().make_server_capacity(8, 1).unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let (tx, rx) = std::sync::mpsc::channel();
    let workers: Vec<_> = [first, second]
        .into_iter()
        .map(|manager| {
            let barrier = Arc::clone(&barrier);
            let tx = tx.clone();
            std::thread::spawn(move || {
                let mut manager = manager.lock();
                barrier.wait();
                tx.send(manager.make_server_capacity(8, 1).unwrap_err().durability)
                    .unwrap();
            })
        })
        .collect();
    for _ in 0..2 {
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            FailureDurability::Capacity
        );
    }
    for worker in workers {
        worker.join().unwrap();
    }
    drop(slot);
}

#[test]
fn server_capacity_evicts_global_lru_without_spreading_a_project_limit_to_peers() {
    let tmp = tempfile::tempdir().unwrap();
    let shared = Arc::new(parking_lot::Mutex::new(ServerCapacity::default()));
    let first = capacity_manager(&shared);
    let second = capacity_manager(&shared);
    let config = Config {
        lsp_max_servers: 8,
        lsp_user_max_servers: Some(2),
        ..Config::default()
    };
    let files: Vec<_> = (0..3)
        .map(|index| {
            let root = tmp.path().join(format!("project{index}"));
            std::fs::create_dir_all(&root).unwrap();
            std::fs::write(root.join("tsconfig.json"), "{}").unwrap();
            let file = root.join("index.ts");
            std::fs::write(&file, "export const value = 1;").unwrap();
            file
        })
        .collect();
    let mut tightened = config.clone();
    tightened.lsp_max_servers = 1;
    assert_eq!(
        settled_lsp(&mut first.lock(), |manager| manager
            .ensure_server_for_file(&files[0], &tightened))
        .len(),
        1
    );
    assert_eq!(
        settled_lsp(&mut second.lock(), |manager| manager
            .ensure_server_for_file(&files[1], &config))
        .len(),
        1
    );
    first
        .lock()
        .clients
        .values_mut()
        .next()
        .unwrap()
        .set_last_used_at_for_test(Instant::now() - Duration::from_secs(300));
    assert_eq!(
        settled_lsp(&mut second.lock(), |manager| manager
            .ensure_server_for_file(&files[2], &config))
        .len(),
        1
    );
    assert_eq!(first.lock().server_count(), 0);
    assert_eq!(second.lock().server_count(), 2);
}

#[test]
fn server_capacity_prepare_failure_releases_its_process_slot() {
    let tmp = tempfile::tempdir().unwrap();
    let shared = Arc::new(parking_lot::Mutex::new(ServerCapacity::default()));
    let manager = capacity_manager(&shared);
    let def = builtin_servers()
        .into_iter()
        .find(|def| matches!(def.kind, ServerKind::TypeScript(_)))
        .unwrap();
    let key = ServerKey {
        kind: def.kind.clone(),
        root: tmp.path().to_path_buf(),
    };
    manager
        .lock()
        .override_binary(def.kind.clone(), tmp.path().join("missing"));
    assert!(matches!(
        manager.lock().begin_file_server_start(
            &def,
            &key,
            &tmp.path().join("index.ts"),
            &Config::default()
        ),
        FileServerStart::Failed(_)
    ));
    assert!(manager.lock().capacity_slots.is_empty());
    assert!(shared
        .lock()
        .slots
        .iter()
        .all(|slot| slot.strong_count() == 0));
}

#[cfg(unix)]
#[test]
fn server_capacity_does_not_evict_pending_requests_and_retries_when_finished() {
    let tmp = tempfile::tempdir().unwrap();
    let mut manager = LspManager::new();
    let mut client = sleeping_rust_client(&manager, tmp.path());
    let request = client
        .start_request::<lsp_types::request::HoverRequest>(idle_hover_params(
            &tmp.path().join("lib.rs"),
        ))
        .unwrap();
    manager.clients.insert(key(tmp.path()), client);
    assert_eq!(
        manager.make_server_capacity(1, 1).unwrap_err().durability,
        FailureDurability::Capacity
    );
    assert_eq!(manager.clients.len(), 1);
    assert!(request.wait(Duration::ZERO).is_err());
    manager.make_server_capacity(1, 1).unwrap();
    assert_eq!(manager.server_count(), 0);
}

#[test]
fn idle_lsp_restart_races_return_start_in_progress_after_three_attempts() {
    for pull in [false, true] {
        let fixture = IdleLspFixture::new();
        let key = fixture
            .ctx
            .lsp()
            .server_key_for_file(&fixture.idle, &fixture.config)
            .unwrap();
        // Both callers acquire the manager to resolve startup, then again to
        // use the client. A pull opens the document in that second acquisition.
        let locks_per_attempt = 2;
        let locks = std::cell::Cell::new(0);
        let detached = std::cell::RefCell::new(None);
        let detachments = std::cell::Cell::new(0);
        let lock = || {
            let call = locks.get();
            assert!(
                call < 3 * locks_per_attempt,
                "restart must stop after three attempts"
            );
            let mut manager = fixture.ctx.lsp();
            // Keep startup successful without spawning another process, then
            // detach the client again before the caller can re-lock to use it.
            if call % locks_per_attempt == 0 {
                if let Some(client) = detached.borrow_mut().take() {
                    assert!(manager.clients.insert(key.clone(), client).is_none());
                }
            }
            if (call + 1) % locks_per_attempt == 0 {
                *detached.borrow_mut() = Some(manager.clients.remove(&key).expect("ready client"));
                detachments.set(detachments.get() + 1);
            }
            locks.set(call + 1);
            manager
        };
        let result = if pull {
            pull_file_diagnostics_unlocked(lock, &fixture.idle, &fixture.config, None).map(|_| ())
        } else {
            ensure_file_open_unlocked(lock, &fixture.idle, &fixture.config).map(|_| ())
        };
        match result {
            Err(LspError::ServerNotReady(reason)) => assert_eq!(reason, START_IN_PROGRESS_REASON),
            other => panic!("expected retryable startup error, got {other:?}"),
        }
        assert_eq!(detachments.get(), 3);
        assert_eq!(locks.get(), 3 * locks_per_attempt);
    }
}

#[test]
fn rust_compiler_recheck_is_not_repeated_after_reopening_unchanged_contents() {
    let tmp = tempfile::tempdir().unwrap();
    let root = canonicalize_for_lsp(tmp.path()).unwrap();
    let key = key(&root);
    let file = root.join("lib.rs");
    std::fs::write(&file, "pub fn first() {}\n").unwrap();
    let mut manager = LspManager::new();
    let client =
        lifecycle_fake_client_with_env(&manager, &key, [("AFT_FAKE_LSP_CHECK_ON_SAVE", "0")]);
    manager.clients.insert(key.clone(), client);
    let servers = std::slice::from_ref(&key);
    manager.open_document_for_servers(&file, servers).unwrap();
    manager
        .unproven_rust_compiler_reports
        .insert((key.clone(), file.clone()), None);
    let initial = manager.documents[&key].version(&file);
    assert!(manager.recheck_unproven_rust_file(&key, &file));
    manager.close_inspect_documents(&file, servers).unwrap();
    manager.open_document_for_servers(&file, servers).unwrap();
    assert!(manager.documents[&key].version(&file) > initial);
    assert!(manager.rust_compiler_report_unproven(&key, &file));
    assert!(!manager.recheck_unproven_rust_file(&key, &file));

    manager.close_inspect_documents(&file, servers).unwrap();
    std::fs::write(&file, "pub fn edited() {}\n").unwrap();
    manager.open_document_for_servers(&file, servers).unwrap();
    assert!(manager.recheck_unproven_rust_file(&key, &file));
}

#[test]
fn idle_lsp_detach_clears_only_reaped_instance_state_and_restarts_during_stop() {
    let fixture = IdleLspFixture::new();
    let old_pid = fixture.pid(&fixture.idle).unwrap();
    // Use the manager's own key: its root is canonical, and on Windows the
    // temp path's short `RUNNER~1` spelling would name a different key.
    let key = fixture
        .ctx
        .lsp()
        .server_key_for_file(&fixture.idle, &fixture.config)
        .unwrap();
    {
        let mut manager = fixture.ctx.lsp();
        seed_instance_state(&mut manager, &key, &fixture.idle);
    }
    let events = fixture.ctx.lsp().event_rx.clone();
    let clients =
        fixture
            .ctx
            .lsp()
            .take_idle_clients(Instant::now(), Duration::from_secs(300), false);
    assert_eq!(clients.len(), 1);
    LspManager::spawn_idle_lsp_reap(clients);
    // The fake server acknowledges receiving Shutdown but withholds its reply.
    // Start the racing request only after the background stop has begun.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let event = events
            .recv_deadline(deadline)
            .expect("retiring server must receive Shutdown");
        if matches!(&event.event, LspEvent::Notification { method, .. } if method == "aft-test/shutdownReceived")
        {
            break;
        }
    }
    fixture.assert_only_busy();
    {
        let manager = fixture.ctx.lsp();
        assert!(!manager.documents.contains_key(&key));
        assert!(!manager
            .diagnostics
            .has_report_for_server_file(&key, &fixture.idle));
        assert!(!manager
            .latest_pull_for_rust
            .contains_key(&(key.clone(), fixture.idle.clone())));
    }
    let response = send_file_request_unlocked::<lsp_types::request::HoverRequest, _>(
        || fixture.ctx.lsp(),
        &fixture.idle,
        &fixture.config,
        idle_hover_params(&fixture.idle),
    )
    .expect("a request racing idle detach must restart the server")
    .unwrap();
    assert!(response.is_some());
    assert_ne!(fixture.pid(&fixture.idle).unwrap(), old_pid);
    fixture.hover(&fixture.idle);
    fixture.hover(&fixture.busy);
    // A retired reader's exit must not remove the new client or its state.
    fixture
        .ctx
        .lsp()
        .handle_event(
            &LspEvent::ServerExited {
                server_kind: key.kind.clone(),
                root: key.root.clone(),
                pid: old_pid,
                reason: ServerExitReason::Eof,
            }
            .into(),
        )
        .unwrap();
    fixture.ctx.lsp().drain_events();
    fixture.hover(&fixture.idle);
}

#[test]
fn idle_lsp_document_notifications_refresh_last_use_but_push_diagnostics_do_not() {
    use lsp_types::notification::{
        DidChangeTextDocument, DidOpenTextDocument, DidSaveTextDocument,
    };
    let fixture = IdleLspFixture::new();
    let uri: lsp_types::Uri = url::Url::from_file_path(&fixture.idle)
        .unwrap()
        .as_str()
        .parse()
        .unwrap();
    let old = Instant::now() - Duration::from_secs(301);
    let mut manager = fixture.ctx.lsp();
    // The manager's own key root, canonical on every platform.
    let server_key = manager
        .server_key_for_file(&fixture.idle, &fixture.config)
        .unwrap();
    let client = manager
        .client_for_file_mut(&fixture.idle, &fixture.config)
        .unwrap();
    macro_rules! refreshed {
        ($send:expr) => {{
            client.set_last_used_at_for_test(old);
            let before = Instant::now();
            $send;
            assert!(client.last_used_at() >= before);
        }};
    }
    refreshed!(client.send_notification::<DidOpenTextDocument>(serde_json::from_value(json!({
        "textDocument": { "uri": uri.as_str(), "languageId": "typescript", "version": 1, "text": "export const n = 1;" }
    })).unwrap()).unwrap());
    refreshed!(client.send_notification::<DidChangeTextDocument>(serde_json::from_value(json!({
        "textDocument": { "uri": uri.as_str(), "version": 2 }, "contentChanges": [{"text": "export const n = 2;"}]
    })).unwrap()).unwrap());
    refreshed!(client
        .send_notification::<DidSaveTextDocument>(
            serde_json::from_value(json!({
                "textDocument": { "uri": uri.as_str() }
            }))
            .unwrap()
        )
        .unwrap());
    refreshed!(client
        .send_full_did_change(&uri, 3, "export const n = 3;")
        .unwrap());
    refreshed!(client.send_did_save_borrowed(&uri, None).unwrap());
    refreshed!(client.record_startup_sync(&uri, true, None));
    client.set_last_used_at_for_test(old);
    manager
        .handle_event(
            &LspEvent::Notification {
                server_kind: server_key.kind,
                root: server_key.root,
                method: "textDocument/publishDiagnostics".into(),
                params: Some(json!({"uri": uri.as_str(), "diagnostics": []})),
            }
            .into(),
        )
        .unwrap();
    assert_eq!(
        manager
            .client_for_file(&fixture.idle, &fixture.config)
            .unwrap()
            .last_used_at(),
        old
    );
}

#[cfg(unix)]
#[test]
fn idle_lsp_never_detaches_an_in_flight_request_even_for_an_idle_root() {
    let tmp = tempfile::tempdir().unwrap();
    let mut manager = LspManager::new();
    let mut client = sleeping_rust_client(&manager, tmp.path());
    let request = client
        .start_request::<lsp_types::request::HoverRequest>(idle_hover_params(
            &tmp.path().join("lib.rs"),
        ))
        .unwrap();
    client.set_last_used_at_for_test(Instant::now() - Duration::from_secs(301));
    manager.clients.insert(key(tmp.path()), client);
    assert!(manager
        .take_idle_clients(Instant::now(), Duration::from_secs(300), true)
        .is_empty());
    assert_eq!(manager.active_client_count(), 1);
    assert!(request.wait(Duration::ZERO).is_err());
    assert_eq!(
        manager
            .take_idle_clients(Instant::now(), Duration::from_secs(300), true)
            .len(),
        1
    );
}

fn key(root: &Path) -> ServerKey {
    ServerKey {
        kind: ServerKind::Rust,
        root: root.to_path_buf(),
    }
}

fn workspace_reply(
    manager: &LspManager,
    key: &ServerKey,
    reply: serde_json::Value,
) -> WorkspacePullReply {
    WorkspacePullReply {
        watched_epoch: None,
        reply: Ok(reply),
        previous_result_ids: manager
            .diagnostics
            .result_ids_for_server(key)
            .into_iter()
            .collect(),
    }
}

fn seed_instance_state(manager: &mut LspManager, key: &ServerKey, file: &Path) {
    manager
        .documents
        .entry(key.clone())
        .or_default()
        .open(file.to_path_buf());
    manager
        .server_binaries
        .insert(key.clone(), "stand-in".into());
    manager
        .diagnostics
        .publish(key.clone(), file.to_path_buf(), Vec::new());
    manager
        .latest_push_for_pull_servers
        .insert((key.clone(), file.to_path_buf()), Vec::new());
    manager
        .latest_pull_for_rust
        .insert((key.clone(), file.to_path_buf()), Vec::new());
    manager.inspect_closed_documents.insert(
        (key.clone(), file.to_path_buf()),
        InspectClosedDocument {
            sequence: 1,
            answered: false,
        },
    );
    manager.watched_file_skip_logged.insert(key.clone());
}

#[test]
fn taking_clients_forgets_every_instance_cache() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("lib.rs");
    let mut manager = LspManager::new();
    seed_instance_state(&mut manager, &key(tmp.path()), &file);
    assert!(manager.take_all_clients().is_empty());
    assert!(manager.latest_push_for_pull_servers.is_empty());
    assert!(manager.latest_pull_for_rust.is_empty());
    assert!(manager.inspect_closed_documents.is_empty());
    assert!(manager.watched_file_skip_logged.is_empty());
    assert!(manager.documents.is_empty());
    assert!(manager.server_binaries.is_empty());
    assert!(manager.diagnostics.is_empty());
}

#[test]
fn exiting_server_forgets_only_its_instance_state() {
    let tmp = tempfile::tempdir().unwrap();
    let exited = key(&tmp.path().join("old"));
    let live = key(&tmp.path().join("live"));
    let file = tmp.path().join("lib.rs");
    let mut manager = LspManager::new();
    for key in [&exited, &live] {
        seed_instance_state(&mut manager, key, &file);
    }
    let _ = manager.handle_event(
        &LspEvent::ServerExited {
            server_kind: exited.kind.clone(),
            root: exited.root.clone(),
            pid: 4242,
            reason: ServerExitReason::Eof,
        }
        .into(),
    );
    assert!(!manager.watched_file_skip_logged.contains(&exited));
    assert!(manager.watched_file_skip_logged.contains(&live));
    for cache in [
        &manager.latest_pull_for_rust,
        &manager.latest_push_for_pull_servers,
    ] {
        assert!(!cache.contains_key(&(exited.clone(), file.clone())));
        assert!(cache.contains_key(&(live.clone(), file.clone())));
    }
    assert!(!manager
        .inspect_closed_documents
        .contains_key(&(exited.clone(), file.clone())));
    assert!(manager
        .inspect_closed_documents
        .contains_key(&(live.clone(), file.clone())));
    assert!(!manager.documents.contains_key(&exited));
    assert!(manager.documents.contains_key(&live));
    assert!(!manager
        .diagnostics
        .has_report_for_server_file(&exited, &file));
    assert!(manager.diagnostics.has_report_for_server_file(&live, &file));
}

#[test]
fn document_pull_ingests_related_full_and_unchanged_reports() {
    let tmp = tempfile::tempdir().unwrap();
    let key = key(tmp.path());
    let requested = tmp.path().join("main.rs");
    let related = tmp.path().join("lib.rs");
    let related_uri = uri_for_path(&related).unwrap().to_string();
    let mut manager = LspManager::new();
    let full = json!({"kind": "full", "items": [], "relatedDocuments": {
        related_uri.clone(): {"kind": "full", "resultId": "related-1", "items": [{
            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
            "message": "dependent error", "severity": 1
        }]}
    }});
    manager.ingest_document_report(
        &key,
        &requested,
        Some(1),
        None,
        serde_json::from_value(full).unwrap(),
    );
    let entries = manager.diagnostics.entries_for_file(&related);
    assert_eq!(
        entries.len(),
        1,
        "dependent-file diagnostics were discarded"
    );
    assert_eq!(entries[0].1.diagnostics[0].message, "dependent error");
    manager.diagnostics.mark_stale_for_file(&related);
    let unchanged = json!({"kind": "unchanged", "resultId": "main-1", "relatedDocuments": {
        related_uri: {"kind": "unchanged", "resultId": "related-1"}
    }});
    manager.ingest_document_report(
        &key,
        &requested,
        Some(1),
        None,
        serde_json::from_value(unchanged).unwrap(),
    );
    assert!(!manager
        .diagnostics
        .has_fresh_report_for_server_file(&key, &related));
    assert_eq!(
        manager.diagnostics.entries_for_file(&related)[0]
            .1
            .diagnostics[0]
            .message,
        "dependent error"
    );
}

#[test]
fn workspace_pull_preserves_versions_and_refreshes_unchanged_reports() {
    let tmp = tempfile::tempdir().unwrap();
    let key = key(tmp.path());
    let file = tmp.path().join("lib.rs");
    let uri = uri_for_path(&file).unwrap();
    let mut manager = LspManager::new();
    std::fs::write(&file, "pub fn f() {}\n").unwrap();
    let file = canonicalize_for_lsp(&file).unwrap();
    manager.sync_document_version_for_test(&key, &file, 7);
    manager
        .finish_workspace_pull(
            &key,
            workspace_reply(
                &manager,
                &key,
                json!({"items": [{
                    "uri": uri, "version": 7, "kind": "full", "resultId": "v1", "items": []
                }]}),
            ),
        )
        .unwrap();
    assert_eq!(
        manager.diagnostics.entries_for_file(&file)[0].1.version,
        Some(7)
    );
    let reply = workspace_reply(
        &manager,
        &key,
        json!({"items": [{
            "uri": uri, "version": 8, "kind": "unchanged", "resultId": "v1"
        }]}),
    );
    manager.diagnostics.mark_stale_for_file(&file);
    manager.sync_document_version_for_test(&key, &file, 8);
    let result = manager.finish_workspace_pull(&key, reply).unwrap();
    assert_eq!(result.files_reported, vec![file.clone()]);
    assert!(manager
        .diagnostics
        .has_fresh_report_for_server_file(&key, &file));
    assert_eq!(
        manager.diagnostics.entries_for_file(&file)[0].1.version,
        Some(8)
    );
    manager.sync_document_version_for_test(&key, &file, 9);
    manager
        .finish_workspace_pull(
            &key,
            workspace_reply(
                &manager,
                &key,
                json!({"items": [{
                    "uri": uri_for_path(&file).unwrap(), "version": 9, "kind": "full", "items": []
                }]}),
            ),
        )
        .unwrap();
    assert!(
        !manager.diagnostics.entries_for_file(&file)[0]
            .1
            .from_document_pull
    );
}

#[test]
fn overflow_drift_scan_hashes_each_path_once_across_servers() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("main.rs");
    std::fs::write(&file, "fn main() {}\n").unwrap();
    let mut manager = LspManager::new();
    for kind in [
        ServerKind::Rust,
        ServerKind::TypeScript(None),
        ServerKind::Biome,
    ] {
        manager
            .documents
            .entry(ServerKey {
                kind,
                root: tmp.path().to_path_buf(),
            })
            .or_default()
            .open(file.clone());
    }
    let before = crate::lsp::document::CONTENT_READS.with(|reads| reads.get());
    assert_eq!(manager.resync_drifted_open_documents(&Config::default()), 0);
    let after = crate::lsp::document::CONTENT_READS.with(|reads| reads.get());
    assert_eq!(
        after - before,
        1,
        "each server independently hashed the same path"
    );
}

#[cfg(unix)]
fn sleeping_rust_client(manager: &LspManager, root: &Path) -> LspClient {
    LspClient::spawn_with_reclaim_root(
        ServerKind::Rust,
        root.to_path_buf(),
        Path::new("sh"),
        &["-c".into(), "exec sleep 60".into()],
        &HashMap::new(),
        manager.event_sender_for_test(),
        manager.child_registry.clone(),
        None,
        false,
    )
    .unwrap()
}

#[cfg(unix)]
#[test]
fn workspace_pull_from_warming_server_remains_provisional() {
    let tmp = tempfile::tempdir().unwrap();
    let key = key(tmp.path());
    let file = tmp.path().join("lib.rs");
    let mut manager = LspManager::new();
    std::fs::write(&file, "pub fn f() {}\n").unwrap();
    let file = canonicalize_for_lsp(&file).unwrap();
    manager.sync_document_version_for_test(&key, &file, 7);
    manager
        .clients
        .insert(key.clone(), sleeping_rust_client(&manager, tmp.path()));
    manager.finish_workspace_pull(&key, workspace_reply(&manager, &key, json!({"items": [{
        "uri": uri_for_path(&file).unwrap(), "version": 7, "kind": "full", "resultId": "v1", "items": []
    }]}))).unwrap();
    assert!(manager.diagnostics.entries_for_file(&file)[0].1.provisional);
    let reply = workspace_reply(
        &manager,
        &key,
        json!({"items": [{
            "uri": uri_for_path(&file).unwrap(), "version": 7, "kind": "unchanged", "resultId": "v1"
        }]}),
    );
    manager.diagnostics.mark_stale_for_file(&file);
    manager.finish_workspace_pull(&key, reply).unwrap();
    assert!(manager.diagnostics.entries_for_file(&file)[0].1.provisional);
    manager
        .clients
        .get_mut(&key)
        .unwrap()
        .set_rust_analyzer_quiescent(true);
    manager.finish_workspace_pull(&key, workspace_reply(&manager, &key, json!({"items": [{
        "uri": uri_for_path(&file).unwrap(), "version": 7, "kind": "unchanged", "resultId": "v1"
    }]}))).unwrap();
    assert!(!manager.diagnostics.entries_for_file(&file)[0].1.provisional);
}

#[cfg(unix)]
#[test]
fn rejected_close_preserves_open_document_and_diagnostics() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("lib.rs");
    std::fs::write(&file, "fn main() {}\n").unwrap();
    let file = canonicalize_for_lsp(&file).unwrap();
    let key = key(tmp.path());
    let mut manager = LspManager::new();
    let client = sleeping_rust_client(&manager, tmp.path());
    client.mark_unresponsive_if_silent(client.received_message_count());
    manager.clients.insert(key.clone(), client);
    seed_instance_state(&mut manager, &key, &file);
    assert!(matches!(
        manager.close_file_for_servers(&file, std::slice::from_ref(&key)),
        Err(LspError::ServerNotReady(_))
    ));
    assert!(
        manager.documents[&key].is_open(&file),
        "unqueued didClose lost the open document"
    );
    assert!(manager.diagnostics.has_report_for_server_file(&key, &file));
}

fn lifecycle_fake_client(manager: &LspManager, key: &ServerKey) -> LspClient {
    lifecycle_fake_client_with_env(manager, key, [])
}

fn lifecycle_fake_client_with_env<'a>(
    manager: &LspManager,
    key: &ServerKey,
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
        &crate::fake_lsp_test_helper::fake_server_binary(),
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

#[test]
fn review_gpt5_event_overflow_preserves_response_and_exit() {
    let tmp = tempfile::tempdir().unwrap();
    let root = canonicalize_for_lsp(tmp.path()).unwrap();
    let file = root.join("index.ts");
    std::fs::write(&file, "export const n = 1;\n").unwrap();
    let key = ServerKey {
        kind: ServerKind::Biome,
        root,
    };
    let mut manager = LspManager::new();
    let mut client =
        lifecycle_fake_client_with_env(&manager, &key, [("AFT_FAKE_LSP_HOVER_EVENT_BURST", "129")]);
    manager
        .diagnostics
        .publish(key.clone(), file.clone(), Vec::new());
    client
        .send_request::<lsp_types::request::HoverRequest>(idle_hover_params(&file))
        .unwrap();
    assert!(
        manager.event_rx.len() <= EVENT_CAP,
        "retained {} ordinary events",
        manager.event_rx.len()
    );
    let instance = client.instance;
    manager.clients.insert(key.clone(), client);
    let drained = manager.drain_events_bounded(1);
    assert!(drained.diagnostics_changed);
    assert!(!manager
        .diagnostics
        .has_fresh_report_for_server_file(&key, &file));
    manager
        .clients
        .get_mut(&key)
        .unwrap()
        .send_request::<lsp_types::request::HoverRequest>(idle_hover_params(&file))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !manager.event_tx.exit_pending_for_test(instance) {
        manager.ingress_rx.recv_deadline(deadline).unwrap();
    }
    assert_eq!(manager.event_rx.len(), EVENT_CAP);
    let drained = manager.drain_events();
    assert!(drained
        .events
        .iter()
        .any(|event| matches!(event, LspEvent::ServerExited { .. })));
    assert!(!manager.clients.contains_key(&key));
}

#[test]
fn event_overflow_does_not_strand_analysis_progress() {
    let (_tmp, mut manager, key) = progress_overflow_client(ServerKind::Python);
    let file = key.root.join("main.py");
    std::fs::write(&file, "n = 1\n").unwrap();
    manager.sync_document_version_for_test(&key, &file, 1);

    send_work_done(&manager, &key, 1, "analysis", "begin");
    publish_for_progress(&manager, &key, 2, &file);
    manager.drain_events();
    assert!(manager.clients[&key].analysis_progress_running());
    assert!(manager.diagnostics.entries_for_file(&file)[0].1.provisional);
    let before_end = manager.diagnostics.current_epoch();

    let end = fill_progress_queue(&manager, &key, 3);
    send_work_done(&manager, &key, end, "analysis", "end");
    manager.drain_events();
    assert!(
        !manager.clients[&key].analysis_progress_running(),
        "analysis finished while the ordinary queue was full"
    );
    assert!(manager.clients[&key].analysis_progress_covered_by_end(before_end));
    assert!(!manager.diagnostics.has_report_for_server_file(&key, &file));

    publish_for_progress(&manager, &key, end + 1, &file);
    manager.drain_events();
    assert!(
        manager.diagnostics.entries_for_file(&file)[0].1.provisional,
        "a report received after the recovered END cannot borrow its authority"
    );

    send_work_done(&manager, &key, end + 2, "next", "begin");
    publish_for_progress(&manager, &key, end + 3, &file);
    send_work_done(&manager, &key, end + 4, "next", "end");
    manager.drain_events();
    assert!(!manager.diagnostics.entries_for_file(&file)[0].1.provisional);
    assert!(manager.has_authoritative_report_for_server(&key));
}

#[test]
fn event_overflow_retains_queued_begin_and_create() {
    let (_tmp, mut manager, key) = progress_overflow_client(ServerKind::Python);
    let file = key.root.join("main.py");
    std::fs::write(&file, "n = 1\n").unwrap();
    manager.sync_document_version_for_test(&key, &file, 1);
    send_work_done(&manager, &key, 1, "active", "begin");
    send_work_done(&manager, &key, 2, "announced", "create");
    let dropped = fill_progress_queue(&manager, &key, 3);
    send_work_done(&manager, &key, dropped, "active", "report");
    manager.drain_events();
    assert!(manager.clients[&key].analysis_progress_running());

    publish_for_progress(&manager, &key, dropped + 1, &file);
    send_work_done(&manager, &key, dropped + 2, "announced", "begin");
    send_work_done(&manager, &key, dropped + 3, "announced", "end");
    manager.drain_events();
    assert!(
        manager.diagnostics.entries_for_file(&file)[0].1.provisional,
        "the active token still has no END"
    );

    send_work_done(&manager, &key, dropped + 4, "pending", "create");
    let dropped = fill_progress_queue(&manager, &key, dropped + 5);
    send_work_done(&manager, &key, dropped, "active", "end");
    manager.drain_events();

    send_work_done(&manager, &key, dropped + 1, "another", "begin");
    publish_for_progress(&manager, &key, dropped + 2, &file);
    send_work_done(&manager, &key, dropped + 3, "another", "end");
    manager.drain_events();
    assert!(
        manager.diagnostics.entries_for_file(&file)[0].1.provisional,
        "the queued create still promises another run"
    );

    send_work_done(&manager, &key, dropped + 4, "pending", "begin");
    send_work_done(&manager, &key, dropped + 5, "pending", "end");
    manager.drain_events();
    assert!(!manager.diagnostics.entries_for_file(&file)[0].1.provisional);
}

#[test]
fn analysis_progress_token_limit_retires_instance_without_certifying() {
    use crate::lsp::analysis_progress::ANALYSIS_TOKEN_LIMIT;

    let (_tmp, mut manager, key) = progress_overflow_client(ServerKind::Python);
    let file = key.root.join("main.py");
    std::fs::write(&file, "n = 1\n").unwrap();
    manager.sync_document_version_for_test(&key, &file, 1);
    publish_for_progress(&manager, &key, 1, &file);
    manager.drain_events();
    assert!(manager.has_authoritative_report_for_server(&key));

    let mut order = 2;
    for index in 0..=ANALYSIS_TOKEN_LIMIT {
        let token = index.to_string();
        send_work_done(&manager, &key, order, &token, "begin");
        send_work_done(&manager, &key, order + 1, &token, "end");
        order += 2;
    }
    manager.drain_events();
    assert!(
        manager.has_client(&key),
        "finished tokens do not occupy slots"
    );

    for index in 0..ANALYSIS_TOKEN_LIMIT {
        send_work_done(&manager, &key, order, &index.to_string(), "begin");
        order += 1;
    }
    manager.drain_events();
    assert!(manager.has_client(&key), "the limit itself is allowed");

    send_work_done(&manager, &key, order, "excess", "begin");
    publish_for_progress(&manager, &key, order + 1, &file);
    manager.drain_events();
    assert!(!manager.has_client(&key));
    assert!(!manager.has_authoritative_report_for_server(&key));
    assert!(!manager.diagnostics.has_report_for_server_file(&key, &file));
}

fn progress_overflow_client(kind: ServerKind) -> (tempfile::TempDir, LspManager, ServerKey) {
    let tmp = tempfile::tempdir().unwrap();
    let key = ServerKey {
        kind,
        root: canonicalize_for_lsp(tmp.path()).unwrap(),
    };
    let mut manager = LspManager::new();
    let client = lifecycle_fake_client(&manager, &key);
    manager.clients.insert(key.clone(), client);
    (tmp, manager, key)
}

fn fill_progress_queue(manager: &LspManager, key: &ServerKey, mut order: u64) -> u64 {
    while manager.event_rx.len() < EVENT_CAP {
        send_work_done(manager, key, order, "ignored", "report");
        order += 1;
    }
    assert_eq!(manager.event_rx.len(), EVENT_CAP);
    order
}

fn send_work_done(manager: &LspManager, key: &ServerKey, order: u64, token: &str, kind: &str) {
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

fn publish_for_progress(manager: &LspManager, key: &ServerKey, order: u64, file: &Path) {
    send_progress_fixture_event(
        manager,
        key,
        order,
        LspEvent::Notification {
            server_kind: key.kind.clone(),
            root: key.root.clone(),
            method: "textDocument/publishDiagnostics".into(),
            params: Some(
                json!({"uri": uri_for_path(file).unwrap(), "version": 1, "diagnostics": []}),
            ),
        },
    );
}

fn send_progress_fixture_event(manager: &LspManager, key: &ServerKey, order: u64, event: LspEvent) {
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
pub(crate) fn drain_flycheck_end_for_maintenance_test(
    manager: &mut LspManager,
    root: &Path,
    storage: &Path,
) -> Instant {
    use super::super::completed_rust_check::{CompletedRustCheck, Runtime};
    use std::sync::atomic::Ordering;
    use std::time::SystemTime;

    let key = ServerKey {
        kind: ServerKind::Rust,
        root: canonicalize_for_lsp(root).unwrap(),
    };
    let mut client =
        lifecycle_fake_client_with_env(manager, &key, [("AFT_FAKE_LSP_SERVER_STATUS", "disabled")]);
    client.set_rust_analyzer_quiescent(true);
    let mut cache = CompletedRustCheck::new(
        &key.root,
        &key.root,
        storage,
        Runtime {
            binary: crate::fake_lsp_test_helper::fake_server_binary(),
            args: Vec::new(),
            env: HashMap::new(),
            options: None,
            launch_env: None,
            validation_gate: None,
        },
    )
    .unwrap();
    // Place BEGIN beyond the timestamp guard for the newly written Cargo inputs.
    cache.begin(SystemTime::now() + Duration::from_secs(3));
    client.completed_rust_check = Some(cache);
    let (_, begins) = client.startup_notification_writer().unwrap();
    begins.store(1, Ordering::Release);
    let instance = client.instance;
    manager.clients.insert(key.clone(), client);
    for kind in ["begin", "end"] {
        manager
            .event_tx
            .send(ReceivedLspEvent {
                event: LspEvent::Notification {
                    server_kind: key.kind.clone(),
                    root: key.root.clone(),
                    method: "$/progress".into(),
                    params: Some(json!({
                        "token": "rust-analyzer/flycheck/0", "value": {"kind": kind}
                    })),
                },
                instance: Some(instance),
                receive_order: None,
            })
            .unwrap();
    }
    manager.drain_events();
    manager.clients[&key]
        .rust_publish_settle_deadline()
        .unwrap()
}

#[test]
fn flycheck_overflow_leaves_no_running_or_completed_check_and_rearms() {
    let tmp = tempfile::tempdir().unwrap();
    let key = ServerKey {
        kind: ServerKind::Rust,
        root: canonicalize_for_lsp(tmp.path()).unwrap(),
    };
    let log = key.root.join("checks.log");
    let mut manager = LspManager::new();
    let mut client = lifecycle_fake_client_with_env(
        &manager,
        &key,
        [
            ("AFT_FAKE_LSP_CHECK_ON_SAVE", "1"),
            ("AFT_FAKE_LSP_CHECK_DROP", "load"),
            ("AFT_FAKE_LSP_CHECK_LOG", log.to_str().unwrap()),
        ],
    );
    client.set_rust_analyzer_quiescent(true);
    let instance = client.instance;
    manager.clients.insert(key.clone(), client);
    let progress = |kind: &str| ReceivedLspEvent {
        event: LspEvent::Notification {
            server_kind: ServerKind::Rust,
            root: key.root.clone(),
            method: "$/progress".into(),
            params: Some(
                json!({"token": "rust-analyzer/flycheck/42", "value": {"kind": kind, "title": "cargo check", "message": "cancelled"}}),
            ),
        },
        instance: Some(instance),
        receive_order: None,
    };
    manager.event_tx.send(progress("begin")).unwrap();
    manager.drain_events();
    assert_eq!(manager.rust_check_state(&key), RustCheckState::Running);

    fill_progress_queue(&manager, &key, 1);
    manager.event_tx.send(progress("end")).unwrap();
    manager.drain_events();
    assert_eq!(
        manager.rust_check_state(&key),
        RustCheckState::Unreported,
        "inspect must not keep waiting for a flycheck run that lost its END"
    );
    assert!(!manager.rust_check_completed_current(&key));

    manager.event_tx.send(progress("end")).unwrap();
    let file = key.root.join("main.rs");
    std::fs::write(&file, "fn main() {}\n").unwrap();
    manager.sync_document_version_for_test(&key, &file, 1);
    publish_for_progress(&manager, &key, 1, &file);
    manager.drain_events();
    assert_eq!(manager.rust_check_state(&key), RustCheckState::Unreported);
    assert!(!manager.rust_check_completed_current(&key));
    assert!(!manager.has_authoritative_report_for_server(&key));

    manager.rearm_unreported_rust_check(&key);
    manager.rearm_unreported_rust_check(&key);
    assert_eq!(manager.rust_check_state(&key), RustCheckState::Running);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !manager.rust_check_completed_current(&key) {
        assert!(
            Instant::now() < deadline,
            "the replacement check did not complete"
        );
        manager.drain_events();
        std::thread::sleep(Duration::from_millis(10));
    }
    let checks = std::fs::read_to_string(log).unwrap();
    assert_eq!(checks.lines().filter(|line| *line == "run").count(), 1);
}

#[test]
fn review_opus9_pulls_all_matching_dynamic_providers_with_their_own_result_ids() {
    let tmp = tempfile::tempdir().unwrap();
    let root = canonicalize_for_lsp(tmp.path()).unwrap();
    let file = root.join("main.py");
    std::fs::write(&file, "n = 1\n").unwrap();
    let log = root.join("pull.log");
    let key = ServerKey {
        kind: ServerKind::Python,
        root,
    };
    let mut manager = LspManager::new();
    let mut client = lifecycle_fake_client_with_env(
        &manager,
        &key,
        [
            ("AFT_FAKE_LSP_MULTIPLE_PULL", "1"),
            ("AFT_FAKE_LSP_DYNAMIC_PULL_LOG", log.to_str().unwrap()),
        ],
    );
    client
        .send_request::<lsp_types::request::HoverRequest>(idle_hover_params(&file))
        .unwrap();
    manager.clients.insert(key.clone(), client);
    manager
        .open_document_for_servers(&file, std::slice::from_ref(&key))
        .unwrap();
    for _ in 0..2 {
        let pull = manager
            .begin_document_pull(&key, &file, Instant::now() + Duration::from_secs(5))
            .unwrap();
        manager.finish_document_pull(pull);
        let entries = manager.diagnostics.entries_for_file(&file);
        let messages = entries
            .into_iter()
            .flat_map(|(_, entry)| entry.diagnostics.iter().map(|item| item.message.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(messages, ["lint", "types"]);
    }
    assert_eq!(
        std::fs::read_to_string(log)
            .unwrap()
            .lines()
            .collect::<Vec<_>>(),
        [
            "provider=lint previous=null",
            "provider=types previous=null",
            "provider=lint previous=\"lint-result\"",
            "provider=types previous=\"types-result\"",
        ]
    );
    std::fs::write(key.root.join("fail-lint"), "").unwrap();
    let pull = manager
        .begin_document_pull(&key, &file, Instant::now() + Duration::from_secs(5))
        .unwrap();
    assert!(matches!(
        manager.finish_document_pull(pull),
        PullFileOutcome::RequestFailed { .. }
    ));
    assert!(!manager
        .diagnostics
        .has_fresh_report_for_server_file(&key, &file));
}

#[test]
fn review_opus8_watched_overflow_invalidates_existing_reports() {
    use crate::lsp::pending_changes::PENDING_LSP_WATCHED_CAP;

    let tmp = tempfile::tempdir().unwrap();
    let root = canonicalize_for_lsp(tmp.path()).unwrap();
    let biome = ServerKey {
        kind: ServerKind::Biome,
        root: root.join("web"),
    };
    let rust = ServerKey {
        kind: ServerKind::Rust,
        root: root.join("rust"),
    };
    for key in [&biome, &rust] {
        std::fs::create_dir(&key.root).unwrap();
    }
    let biome_file = biome.root.join("index.ts");
    let rust_file = rust.root.join("lib.rs");
    std::fs::write(&biome_file, "export const n = 1;\n").unwrap();
    std::fs::write(&rust_file, "pub const N: usize = 1;\n").unwrap();
    std::fs::write(biome.root.join("biome.json"), "{}").unwrap();
    let mut manager = LspManager::new();
    let mut old_biome =
        lifecycle_fake_client_with_env(&manager, &biome, [("AFT_FAKE_LSP_HOLD_HOVER", "1")]);
    let old_biome_instance = old_biome.instance;
    let rust_client = lifecycle_fake_client(&manager, &rust);
    let rust_instance = rust_client.instance;
    let held = old_biome
        .start_request::<lsp_types::request::HoverRequest>(idle_hover_params(&biome_file))
        .unwrap();
    manager.clients.insert(biome.clone(), old_biome);
    manager.clients.insert(rust.clone(), rust_client);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let event = manager.event_rx.recv_deadline(deadline).unwrap();
        if matches!(&event.event, LspEvent::Notification { method, .. } if method == "custom/hoverHeld")
        {
            break;
        }
        manager.handle_event(&event).ok();
    }
    manager
        .diagnostics
        .publish(biome.clone(), biome_file.clone(), Vec::new());
    manager
        .diagnostics
        .publish(rust.clone(), rust_file.clone(), Vec::new());
    let slot = manager.pending_change_slot();
    let events = |dropped: PathBuf| {
        (0..PENDING_LSP_WATCHED_CAP)
            .map(|index| {
                (
                    biome.root.join(format!("{index}.ts")),
                    FileChangeType::CREATED,
                )
            })
            .chain(std::iter::once((dropped, FileChangeType::CREATED)))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        slot.queue_watched_for_test(&events(biome.root.join("overflow.ts"))),
        PENDING_LSP_WATCHED_CAP
    );
    manager.deliver_pending_watched_backlog();
    assert!(!manager
        .diagnostics
        .has_fresh_report_for_server_file(&biome, &biome_file));
    assert!(manager
        .diagnostics
        .has_fresh_report_for_server_file(&rust, &rust_file));
    assert_eq!(manager.clients[&biome].instance, old_biome_instance);
    let client = manager.clients.get_mut(&biome).unwrap();
    assert!(matches!(
        client.start_request::<lsp_types::request::HoverRequest>(idle_hover_params(&biome_file)),
        Err(LspError::ServerNotReady(_))
    ));
    client
        .send_notification::<lsp_types::notification::DidChangeConfiguration>(
            lsp_types::DidChangeConfigurationParams {
                settings: json!({}),
            },
        )
        .unwrap();
    held.wait(Duration::from_secs(5)).unwrap();
    manager.deliver_pending_watched_backlog();
    assert!(!manager.clients.contains_key(&biome));

    manager.override_binary(
        ServerKind::Biome,
        crate::fake_lsp_test_helper::fake_server_binary(),
    );
    let config = Config {
        project_root: Some(root.clone()),
        ..Default::default()
    };
    let _ = settled_lsp(&mut manager, |manager| {
        manager.ensure_server_for_file(&biome_file, &config)
    });
    assert_ne!(manager.clients[&biome].instance, old_biome_instance);

    assert_eq!(
        slot.queue_watched_for_test(&events(root.join("unknown.ts"))),
        PENDING_LSP_WATCHED_CAP
    );
    manager.deliver_pending_watched_backlog();
    assert!(!manager
        .diagnostics
        .has_fresh_report_for_server_file(&rust, &rust_file));
    assert!(!manager.clients.contains_key(&biome));
    assert_eq!(manager.clients[&rust].instance, rust_instance);
    manager
        .clients
        .get_mut(&rust)
        .unwrap()
        .send_request::<lsp_types::request::HoverRequest>(idle_hover_params(&rust_file))
        .unwrap();
    let drained = manager.drain_events();
    assert_eq!(drained.events.iter().filter(|event| {
        matches!(event, LspEvent::Notification { method, .. } if method == "custom/reloadWorkspace")
    }).count(), 1);
}

fn reject_fixture_writes(client: &mut LspClient, file: &Path) {
    // Wait for the fake's hover reply before marking it unresponsive. Any
    // earlier notification received after the mark would clear it.
    client
        .send_request::<lsp_types::request::HoverRequest>(idle_hover_params(file))
        .unwrap();
    client.mark_unresponsive_if_silent(client.received_message_count());
}

#[test]
fn scoped_close_rejected_before_queue_keeps_the_open_document() {
    let tmp = tempfile::tempdir().unwrap();
    let root = canonicalize_for_lsp(tmp.path()).unwrap();
    let file = root.join("index.ts");
    std::fs::write(&file, "export const n = 1;\n").unwrap();
    let key = ServerKey {
        kind: ServerKind::Biome,
        root,
    };
    let mut manager = LspManager::new();
    let client = lifecycle_fake_client(&manager, &key);
    manager.clients.insert(key.clone(), client);
    let keys = std::slice::from_ref(&key);
    assert_eq!(
        manager.open_document_for_servers(&file, keys).unwrap(),
        keys
    );
    manager
        .diagnostics
        .publish(key.clone(), file.clone(), Vec::new());
    reject_fixture_writes(manager.clients.get_mut(&key).unwrap(), &file);

    assert!(matches!(
        manager.close_inspect_documents(&file, keys),
        Err(LspError::ServerNotReady(_))
    ));
    assert!(manager.documents[&key].is_open(&file));
    assert!(manager
        .open_document_for_servers(&file, keys)
        .unwrap()
        .is_empty());
    assert!(manager.diagnostics.has_report_for_server_file(&key, &file));
}

#[test]
fn failed_first_server_does_not_leave_later_server_on_pre_edit_content() {
    let tmp = tempfile::tempdir().unwrap();
    let root = canonicalize_for_lsp(tmp.path()).unwrap();
    let file = root.join("index.ts");
    std::fs::write(&file, "export const n = 1;\n").unwrap();
    let first = ServerKey {
        kind: ServerKind::TypeScript(None),
        root: root.clone(),
    };
    let second = ServerKey {
        kind: ServerKind::Biome,
        root,
    };
    let keys = vec![first.clone(), second.clone()];
    let mut manager = LspManager::new();
    for key in &keys {
        let client = lifecycle_fake_client(&manager, key);
        manager.clients.insert(key.clone(), client);
    }
    assert_eq!(
        manager.open_document_for_servers(&file, &keys).unwrap(),
        keys
    );
    manager
        .diagnostics
        .publish(second.clone(), file.clone(), Vec::new());
    assert!(manager
        .diagnostics
        .has_fresh_report_for_server_file(&second, &file));
    let old_version = manager.documents[&second].version(&file).unwrap();
    reject_fixture_writes(manager.clients.get_mut(&first).unwrap(), &file);
    let edited = "export const n = 2;\n";
    std::fs::write(&file, edited).unwrap();

    assert!(matches!(
        manager.notify_file_changed_for_server_keys(file.clone(), edited, keys),
        Err(LspError::ServerNotReady(_))
    ));
    assert_eq!(
        manager.documents[&second].version(&file),
        Some(old_version + 1)
    );
    assert!(!manager
        .diagnostics
        .has_fresh_report_for_server_file(&second, &file));
}

#[test]
fn per_file_pulls_close_temporary_documents_at_each_open_version() {
    let tmp = tempfile::tempdir().unwrap();
    let root = canonicalize_for_lsp(tmp.path()).unwrap();
    let config = Config {
        project_root: Some(root.clone()),
        ..Config::default()
    };
    let key = key(&root);
    let manager = parking_lot::Mutex::new(LspManager::new());
    manager.lock().override_binary(
        ServerKind::Rust,
        crate::fake_lsp_test_helper::fake_server_binary(),
    );
    manager.lock().set_extra_env("AFT_FAKE_LSP_PULL", "1");
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = 'temporary-pulls'\nversion = '0.0.0'\n",
    )
    .unwrap();
    std::fs::create_dir(root.join("src")).unwrap();
    for (name, initial_version) in [("lib.rs", 1), ("other.rs", 2)] {
        let file = root.join("src").join(name);
        std::fs::write(&file, "pub fn f() {}\n").unwrap();
        let results =
            pull_file_diagnostics_unlocked_inner(|| manager.lock(), &file, &config, None, false)
                .unwrap();
        let mut manager = manager.lock();
        assert_eq!(results.len(), 1);
        assert!(matches!(results[0].outcome, PullFileOutcome::Full { .. }));
        assert_eq!(
            manager.documents[&key].version(&file),
            Some(initial_version)
        );
        manager.close_documents_opened_for_pulls(&file, &results);
        assert!(
            !manager.documents[&key].is_open(&file),
            "{name} stayed open"
        );
    }

    let file = root.join("src/edited.rs");
    std::fs::write(&file, "pub fn f() {}\n").unwrap();
    let pulls = manager
        .lock()
        .begin_file_pulls(&file, &config, None)
        .unwrap();
    let edited = "pub fn edited() {}\n";
    std::fs::write(&file, edited).unwrap();
    manager
        .lock()
        .notify_file_changed_for_server_keys(file.clone(), edited, vec![key.clone()])
        .unwrap();
    let replies = pulls
        .into_iter()
        .map(DocumentPull::wait)
        .collect::<Vec<_>>();
    let mut manager = manager.lock();
    let results = replies
        .into_iter()
        .map(|pull| PullFileResult {
            server_key: pull.key.clone(),
            opened_for_pull: pull.opened_for_pull,
            outcome: manager.finish_document_pull(pull),
        })
        .collect::<Vec<_>>();
    manager.close_documents_opened_for_pulls(&file, &results);
    assert_eq!(manager.documents[&key].version(&file), Some(4));
}

#[test]
fn lsp_inspect_keeps_push_only_document_open_to_certify_a_later_push() {
    let dir = tempfile::tempdir().unwrap();
    let root = canonicalize_for_lsp(dir.path()).unwrap();
    let file = root.join("src").join("lib.rs");
    std::fs::create_dir(root.join("src")).unwrap();
    std::fs::write(&file, "pub fn f() {}\n").unwrap();
    std::fs::write(root.join("Cargo.toml"), "[package]\nname = 'push-only'\n").unwrap();
    let key = key(&root);
    let ctx = crate::context::AppContext::new(
        crate::context::default_language_provider_factory(),
        Config {
            project_root: Some(root),
            ..Config::default()
        },
    );
    {
        let mut manager = ctx.lsp();
        manager.override_binary(
            key.kind.clone(),
            crate::fake_lsp_test_helper::fake_server_binary(),
        );
        manager.set_extra_env("AFT_FAKE_LSP_PULL", "0");
        manager.set_extra_env("AFT_FAKE_LSP_DISABLE_PUSH", "1");
    }
    let req = serde_json::from_value(json!({"id":"inspect", "command":"lsp_inspect", "file":file}))
        .unwrap();
    let inspect = crate::commands::lsp_inspect::handle_lsp_inspect(&req, &ctx);
    assert_eq!(
        inspect.data["pull_results"][0]["status"],
        "pull_not_supported"
    );

    let mut manager = ctx.lsp();
    let document = manager.documents[&key]
        .entry(&file)
        .expect("push-only document stays open");
    let (version, received_after_sync) = (document.version, document.sync_order.unwrap() + 1);
    manager.handle_publish_diagnostics_ordered(
        key.kind.clone(),
        key.root.clone(),
        &json!({
            "uri": uri_for_path(&file).unwrap(),
            "diagnostics": [{
                "range": {"start":{"line":0,"character":0},"end":{"line":0,"character":3}},
                "severity": 1, "code": "late", "message": "late push-only diagnostic"
            }]
        }),
        Some(received_after_sync),
    );
    let entry = manager
        .diagnostics
        .report_for_server_file(&key, &file)
        .unwrap();
    assert_eq!(entry.version, Some(version));
    assert_eq!(entry.content_hash, file_content_hash(&file));
    assert_eq!(
        manager.get_diagnostics_for_file(&file)[0].message,
        "late push-only diagnostic"
    );
}

#[test]
fn unversioned_closed_push_masks_status_until_a_certified_pull() {
    let dir = tempfile::tempdir().unwrap();
    let root = canonicalize_for_lsp(dir.path()).unwrap();
    let file = root.join("broken.go");
    let clean = root.join("clean.go");
    std::fs::write(&file, "package main\nvar n int = \"error\"\n").unwrap();
    std::fs::write(&clean, "package main\n").unwrap();
    let key = ServerKey {
        kind: ServerKind::Go,
        root: root.clone(),
    };
    let ctx = crate::context::AppContext::new(
        crate::context::default_language_provider_factory(),
        Config::default(),
    );
    ctx.set_canonical_cache_root(root);
    ctx.lsp().store_project_applicability(Default::default());
    let pull = |file: &Path, items: serde_json::Value, count| {
        let mut manager = ctx.lsp();
        let version = manager
            .documents
            .entry(key.clone())
            .or_default()
            .open(file.to_path_buf());
        let report =
            decode_document_diagnostic_report(json!({"kind":"full", "items":items})).unwrap();
        assert!(matches!(
            manager.ingest_document_report(&key, file, Some(version), None, report),
            PullFileOutcome::Full { diagnostic_count } if diagnostic_count == count
        ));
        manager
            .close_inspect_documents(file, std::slice::from_ref(&key))
            .unwrap();
        assert!(!manager.documents[&key].is_open(file));
    };
    let status = || {
        let values = ctx.status_bar_count_values();
        (values.errors, values.warnings, values.diagnostics_reason)
    };
    let error = json!([{
        "range": {"start":{"line":1,"character":12},"end":{"line":1,"character":19}},
        "severity": 1, "message": "type error"
    }]);
    // The clean row would keep the counts numeric if the unverified error
    // were merely omitted.
    pull(&clean, json!([]), 0);
    pull(&file, error.clone(), 1);
    assert_eq!(status(), (Some(1), Some(0), None));

    for message in ["first unverified error", "new unverified error"] {
        let mut manager = ctx.lsp();
        manager.handle_publish_diagnostics(
            key.kind.clone(),
            key.root.clone(),
            &json!({
                "uri": uri_for_path(&file).unwrap(), "diagnostics": [{
                    "range": {"start":{"line":1,"character":12},"end":{"line":1,"character":19}},
                    "severity": 1, "message": message
                }]
            }),
        );
        let entry = manager
            .diagnostics
            .report_for_server_file(&key, &file)
            .unwrap();
        assert!(entry.stale && entry.version.is_none());
        assert_eq!(entry.diagnostics[0].message, message);
        drop(manager);
        assert_eq!(status(), (None, None, Some("producer_reports_pending")));
        assert_eq!(
            status(),
            (None, None, Some("producer_reports_pending")),
            "cached counts"
        );
    }

    pull(&file, error, 1);
    assert_eq!(status(), (Some(1), Some(0), None));
}

#[test]
fn owned_pull_ingestion_reuses_diagnostic_and_result_id_allocations() {
    let tmp = tempfile::tempdir().unwrap();
    let key = key(tmp.path());
    let file = tmp.path().join("lib.rs");
    let report: lsp_types::FullDocumentDiagnosticReport = serde_json::from_value(json!({
        "resultId": "owned-result-id",
        "items": [{
            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
            "message": "an owned diagnostic message", "source": "owned-source", "code": "owned-code"
        }]
    }))
    .unwrap();
    let allocations = [
        report.result_id.as_ref().unwrap().as_ptr(),
        report.items[0].message.as_ptr(),
        report.items[0].source.as_ref().unwrap().as_ptr(),
    ];
    let mut manager = LspManager::new();
    manager.ingest_document_report_kind(
        &key,
        &file,
        Some(1),
        None,
        true,
        lsp_types::DocumentDiagnosticReportKind::Full(report),
    );
    let entries = manager.diagnostics.entries_for_file(&file);
    let stored = &entries[0].1;
    // Each nonempty string must retain its allocation across the ownership
    // transfer. Cloning the report would allocate replacement strings while
    // the originals are still live, so this also measures the copies avoided.
    let retained = [
        stored.result_id.as_ref().unwrap().as_ptr(),
        stored.diagnostics[0].message.as_ptr(),
        stored.diagnostics[0].source.as_ref().unwrap().as_ptr(),
    ];
    assert_eq!(
        allocations, retained,
        "owned pull strings were unnecessarily cloned"
    );
}
