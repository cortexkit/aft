use super::*;
use crate::config::UserServerDef;
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

const WAIT: Duration = Duration::from_secs(30);

thread_local! {
    static MANAGER_LOCK_HELD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn locked<R>(f: &Fixture, operation: impl FnOnce(&mut LspManager) -> R) -> R {
    let mut manager = f.manager.lock();
    MANAGER_LOCK_HELD.set(true);
    let result = operation(&mut manager);
    MANAGER_LOCK_HELD.set(false);
    result
}

struct Fixture {
    _root: tempfile::TempDir,
    config: Config,
    file: PathBuf,
    manager: Arc<parking_lot::Mutex<LspManager>>,
    snapshot: ApplicableServerSnapshot,
    key: ServerKey,
}

fn fixture() -> Fixture {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("marker"), "marker").unwrap();
    let file = root.path().join("source.reserved");
    std::fs::write(&file, "content").unwrap();
    let mut frame = Vec::new();
    super::super::transport::write_message(
        &mut frame,
        r#"{"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}}"#,
    )
    .unwrap();
    let script = root.path().join("server.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nIFS= read -r header\nprintf '%s' '{}'\nwhile IFS= read -r line; do :; done\n",
            String::from_utf8(frame).unwrap(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let config = Config {
        project_root: Some(root.path().to_path_buf()),
        lsp_servers: vec![UserServerDef {
            id: "reserved".into(),
            extensions: vec!["reserved".into()],
            binary: "reserved".into(),
            args: Vec::new(),
            root_markers: vec!["marker".into()],
            env: HashMap::new(),
            initialization_options: None,
            disabled: false,
        }],
        ..Config::default()
    };
    let mut manager = LspManager::new();
    manager.override_binary(ServerKind::Custom(Arc::from("reserved")), script);
    let definition = servers_for_file(&file, &config).remove(0);
    let key = server_key_for_definition(&definition, &file, &config).unwrap();
    let snapshot = ApplicableServerSnapshot {
        server_keys: vec![key.clone()],
        candidates: vec![ApplicableServerCandidate {
            key: key.clone(),
            definition,
            source_file: file.clone(),
        }],
        ..ApplicableServerSnapshot::default()
    };
    let manager = Arc::new(parking_lot::Mutex::new(manager));
    manager.lock().startup_owner = Arc::downgrade(&manager);
    Fixture {
        _root: root,
        config,
        file,
        manager,
        snapshot,
        key,
    }
}

struct Pause {
    entered: Receiver<()>,
    resume: Sender<()>,
    spawns: Arc<AtomicUsize>,
    under_lock: Arc<AtomicBool>,
}

fn pause_first_spawn(f: &Fixture) -> Pause {
    let (entered_tx, entered) = unbounded();
    let (resume, resume_rx) = unbounded();
    let spawns = Arc::new(AtomicUsize::new(0));
    let under_lock = Arc::new(AtomicBool::new(false));
    let count = Arc::clone(&spawns);
    let locked = Arc::clone(&under_lock);
    f.manager.lock().spawn_hook = Some(Arc::new(move || {
        // Only a guard held by the spawning thread violates the lock boundary;
        // another caller is allowed to inspect the manager during startup.
        if MANAGER_LOCK_HELD.get() {
            locked.store(true, Ordering::SeqCst);
        }
        if count.fetch_add(1, Ordering::SeqCst) == 0 {
            entered_tx.send(()).unwrap();
            resume_rx.recv_timeout(WAIT).unwrap();
        }
    }));
    Pause {
        entered,
        resume,
        spawns,
        under_lock,
    }
}

#[test]
fn locked_and_unlocked_starts_share_one_reservation() {
    let f = fixture();
    let pause = pause_first_spawn(&f);
    std::thread::scope(|scope| {
        let running = scope.spawn(|| {
            ensure_server_for_file_detailed_unlocked(|| f.manager.lock(), &f.file, &f.config)
        });
        pause.entered.recv_timeout(WAIT).unwrap();
        let file = locked(&f, |manager| {
            manager.ensure_server_for_file_detailed(&f.file, &f.config)
        });
        let area = locked(&f, |manager| {
            manager.start_applicable_servers(&f.snapshot, &f.config)
        });
        pause.resume.send(()).unwrap();
        let completed = running.join().unwrap();
        assert_eq!(pause.spawns.load(Ordering::SeqCst), 1, "duplicate spawn");
        assert_eq!(
            server_attempt_result_reason(&file.attempts[0].result),
            "start_in_progress"
        );
        assert!(file.successful.is_empty());
        assert!(area.successful.is_empty());
        assert!(area.failures.is_empty());
        assert_eq!(completed.successful, vec![f.key.clone()]);
        assert!(
            !pause.under_lock.load(Ordering::SeqCst),
            "spawn held manager lock"
        );
    });
}

#[test]
fn locked_cold_start_never_spawns_under_manager_lock() {
    for area in [false, true] {
        let f = fixture();
        let pause = pause_first_spawn(&f);
        pause.resume.send(()).unwrap();
        if area {
            locked(&f, |manager| {
                manager.start_applicable_servers(&f.snapshot, &f.config)
            });
        } else {
            locked(&f, |manager| {
                manager.ensure_server_for_file_detailed(&f.file, &f.config)
            });
        }
        pause.entered.recv_timeout(WAIT).unwrap();
        assert!(
            !pause.under_lock.load(Ordering::SeqCst),
            "spawn held manager lock"
        );
        let completed =
            ensure_server_for_file_detailed_unlocked(|| f.manager.lock(), &f.file, &f.config);
        assert_eq!(completed.successful, vec![f.key.clone()]);
    }
}

#[test]
fn waiter_sees_pending_then_the_real_result() {
    let f = fixture();
    let pause = pause_first_spawn(&f);
    let initial = locked(&f, |manager| {
        manager.ensure_server_for_file_detailed(&f.file, &f.config)
    });
    pause.entered.recv_timeout(WAIT).unwrap();
    let pending = locked(&f, |manager| {
        manager.ensure_server_for_file_detailed(&f.file, &f.config)
    });
    assert!(
        matches!(&initial.attempts[0].result, ServerAttemptResult::InProgress { server_key } if server_key == &f.key)
    );
    assert_eq!(START_IN_PROGRESS_STATUS, "start_in_progress");
    assert_eq!(
        START_IN_PROGRESS_REASON,
        "server startup is in progress; retry shortly"
    );
    assert_eq!(
        pending.attempts[0].result.failure_reason(),
        START_IN_PROGRESS_REASON
    );
    let response =
        crate::commands::lsp_diagnostics::startup_unavailable_response(&f.file, &pending);
    assert_eq!(response["complete"], false);
    assert_eq!(response["pending_servers"], serde_json::json!(["reserved"]));
    assert_eq!(
        response["unchecked_files"],
        serde_json::json!([f.file.display().to_string()])
    );
    assert_eq!(
        response["lsp_servers_used"][0]["status"],
        "start_in_progress"
    );
    let area = locked(&f, |manager| {
        manager.start_applicable_servers(&f.snapshot, &f.config)
    });
    assert_eq!(area.in_progress, vec![f.key.clone()]);
    assert!(area.successful.is_empty());
    assert!(!f.manager.lock().failed_spawns.contains_key(&f.key));
    assert!(!f.manager.lock().transient_backoff.contains_key(&f.key));
    pause.resume.send(()).unwrap();
    let ready = ensure_server_for_file_detailed_unlocked(|| f.manager.lock(), &f.file, &f.config);
    assert_eq!(ready.successful, vec![f.key.clone()]);
    assert_eq!(
        server_attempt_result_reason(&ready.attempts[0].result),
        "ok"
    );
    assert_eq!(
        ServerAttemptResult::BinaryNotInstalled {
            binary: "example-ls".into()
        }
        .failure_reason(),
        "example-ls is unavailable"
    );
    assert_eq!(pause.spawns.load(Ordering::SeqCst), 1);
}

#[test]
fn failed_spawn_releases_reservation_for_retry_and_all_waiters() {
    let f = fixture();
    let binary = f.manager.lock().binary_overrides[&f.key.kind].clone();
    let original = std::fs::read(&binary).unwrap();
    let clock_offset = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let clock = Arc::clone(&clock_offset);
    let now = Instant::now();
    f.manager
        .lock()
        .set_retry_clock(RetryClock::from_fn(move || {
            now + Duration::from_secs(clock.load(Ordering::SeqCst))
        }));
    let pause = pause_first_spawn(&f);
    locked(&f, |manager| {
        manager.ensure_server_for_file_detailed(&f.file, &f.config)
    });
    pause.entered.recv_timeout(WAIT).unwrap();
    // An executable image error occurs after binary resolution and is transient.
    std::fs::write(&binary, "invalid executable image\n").unwrap();
    std::thread::scope(|scope| {
        let one = scope.spawn(|| {
            ensure_server_for_file_detailed_unlocked(|| f.manager.lock(), &f.file, &f.config)
        });
        let two = scope.spawn(|| {
            ensure_server_for_file_detailed_unlocked(|| f.manager.lock(), &f.file, &f.config)
        });
        let mut manager = f.manager.lock();
        let signal = Arc::clone(&manager.starting[&f.key].signal);
        pause.resume.send(()).unwrap();
        assert!(signal.wait_until(Instant::now() + WAIT));
        manager.publish_completed_starts();
        assert!(manager.starting.is_empty());
        drop(manager);
        let one = one.join().unwrap();
        let two = two.join().unwrap();
        assert!(matches!(
            one.attempts[0].result,
            ServerAttemptResult::SpawnFailed { .. }
        ));
        assert_eq!(
            server_attempt_result_reason(&one.attempts[0].result),
            server_attempt_result_reason(&two.attempts[0].result)
        );
    });
    assert!(f.manager.lock().starting.is_empty());
    assert_eq!(pause.spawns.load(Ordering::SeqCst), 1);
    std::fs::write(&binary, original).unwrap();
    clock_offset.store(3600, Ordering::SeqCst);
    let retry = ensure_server_for_file_detailed_unlocked(|| f.manager.lock(), &f.file, &f.config);
    assert_eq!(retry.successful, vec![f.key.clone()]);
    assert_eq!(pause.spawns.load(Ordering::SeqCst), 2);
}

#[test]
fn closing_project_during_startup_reaps_child_and_never_installs_it() {
    for shutdown in [false, true] {
        let f = fixture();
        let (spawned_tx, spawned_rx) = unbounded();
        let (resume_tx, resume_rx) = unbounded();
        f.manager.lock().spawned_hook = Some(Arc::new(move |pid| {
            spawned_tx.send(pid).unwrap();
            resume_rx.recv_timeout(WAIT).unwrap();
        }));
        locked(&f, |manager| {
            manager.ensure_server_for_file_detailed(&f.file, &f.config)
        });
        let pid = spawned_rx.recv_timeout(WAIT).unwrap();
        let signal = Arc::clone(&f.manager.lock().starting[&f.key].signal);
        if shutdown {
            f.manager.lock().shutdown_all();
        } else {
            f.manager.lock().quiesce();
        }
        resume_tx.send(()).unwrap();
        assert!(signal.cancelled.load(Ordering::SeqCst));
        assert!(signal.wait_until(Instant::now() + WAIT));
        // waitpid blocks in the kernel, not a timing loop; the client's reaper
        // may already have consumed the exit status, which also proves exit.
        let waited = unsafe { libc::waitpid(pid as i32, std::ptr::null_mut(), 0) };
        assert!(
            waited == pid as i32
                || (waited == -1
                    && std::io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD))
        );
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
        f.manager.lock().publish_completed_starts();
        assert_eq!(f.manager.lock().server_count(), 0);
        assert!(f.manager.lock().child_registry.pids().is_empty());
        if !shutdown {
            let result = locked(&f, |manager| {
                manager.ensure_server_for_file_detailed(&f.file, &f.config)
            });
            assert!(matches!(
                result.attempts[0].result,
                ServerAttemptResult::ProjectClosed
            ));
        }
    }
}

#[test]
fn cancelled_start_cannot_publish_over_a_new_reservation() {
    let f = fixture();
    let (finished_tx, finished_rx) = unbounded();
    f.manager.lock().startup_finished_hook = Some(Arc::new(move || {
        finished_tx.send(()).unwrap();
    }));
    let pause = pause_first_spawn(&f);
    let spawned = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&spawned);
    f.manager.lock().spawned_hook = Some(Arc::new(move |_| {
        count.fetch_add(1, Ordering::SeqCst);
    }));
    locked(&f, |manager| {
        manager.ensure_server_for_file_detailed(&f.file, &f.config)
    });
    pause.entered.recv_timeout(WAIT).unwrap();
    let old = Arc::clone(&f.manager.lock().starting[&f.key].signal);
    f.manager.lock().quiesce();
    assert!(f.manager.lock().starting.is_empty());
    f.manager.lock().resume();
    let ready = ensure_server_for_file_detailed_unlocked(|| f.manager.lock(), &f.file, &f.config);
    assert_eq!(ready.successful, vec![f.key.clone()]);
    let pid = f.manager.lock().clients[&f.key].child_pid();
    finished_rx.recv_timeout(WAIT).unwrap();
    pause.resume.send(()).unwrap();
    finished_rx.recv_timeout(WAIT).unwrap();
    assert!(old.cancelled.load(Ordering::SeqCst));
    assert!(matches!(
        *old.outcome.lock(),
        Some(ServerAttemptResult::ProjectClosed)
    ));
    f.manager.lock().publish_completed_starts();
    assert_eq!(f.manager.lock().clients[&f.key].child_pid(), pid);
    assert_eq!(spawned.load(Ordering::SeqCst), 1);
}

#[test]
fn in_progress_is_not_a_cached_executable_failure() {
    let f = fixture();
    let result = f.manager.lock().record_failed_spawn(
        &f.key,
        ServerAttemptResult::InProgress {
            server_key: f.key.clone(),
        },
        FailureDurability::Transient,
    );
    assert!(matches!(result, ServerAttemptResult::InProgress { .. }));
    assert!(f.manager.lock().failed_spawns.is_empty());
    assert!(f.manager.lock().transient_backoff.is_empty());
}

fn notification_fixture() -> Fixture {
    let f = fixture();
    let binary = f.manager.lock().binary_overrides[&f.key.kind].clone();
    std::fs::write(&binary, r#"#!/usr/bin/env python3
import json, os, sys
seen = []
def write_message(message):
    body = json.dumps(message).encode()
    sys.stdout.buffer.write(('Content-Length: %d\r\n\r\n' % len(body)).encode() + body)
    sys.stdout.buffer.flush()
def register_watched_files():
    write_message({'jsonrpc': '2.0', 'id': 'startup-watches', 'method': 'client/registerCapability', 'params': {
        'registrations': [{'id': 'startup-watches', 'method': 'workspace/didChangeWatchedFiles',
                           'registerOptions': {'watchers': [{'globPattern': '**/*'}]}}]}})
while True:
    headers = {}
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            sys.exit(0)
        if line in (b'\r\n', b'\n'):
            break
        key, value = line.decode().split(':', 1)
        headers[key.lower()] = value.strip()
    message = json.loads(sys.stdin.buffer.read(int(headers['content-length'])))
    method = message.get('method')
    if method == 'initialized' and os.environ.get('AFT_STARTUP_WATCHES') == '1':
        register_watched_files()
    if method == 'test/notifications' and message.get('params', {}).get('register_watches'):
        register_watched_files()
    if method in ('textDocument/didOpen', 'textDocument/didChange', 'workspace/didChangeWatchedFiles'):
        seen.append(message)
        if method != 'workspace/didChangeWatchedFiles' and os.environ.get('AFT_STARTUP_PUSH') == '1':
            document = message['params']['textDocument']
            write_message({'jsonrpc': '2.0', 'method': 'textDocument/publishDiagnostics', 'params': {
                'uri': document['uri'], 'version': document['version'], 'diagnostics': [{
                    'range': {'start': {'line': 0, 'character': 0}, 'end': {'line': 0, 'character': 1}},
                    'severity': 1, 'message': 'startup replay diagnostic'}]}})
    if 'id' in message and method is not None:
        result = {'capabilities': {}} if method == 'initialize' else seen if method == 'test/notifications' else None
        write_message({'jsonrpc': '2.0', 'id': message['id'], 'result': result})
    if method == 'exit':
        break
"#).unwrap();
    f
}

enum RecordedNotifications {}
impl lsp_types::request::Request for RecordedNotifications {
    type Params = serde_json::Value;
    type Result = Vec<serde_json::Value>;
    const METHOD: &'static str = "test/notifications";
}

fn recorded_notifications(f: &Fixture) -> Vec<serde_json::Value> {
    recorded_notifications_with(f, serde_json::json!({}))
}

fn recorded_notifications_with(f: &Fixture, params: serde_json::Value) -> Vec<serde_json::Value> {
    let request = f
        .manager
        .lock()
        .clients
        .get_mut(&f.key)
        .unwrap()
        .start_request::<RecordedNotifications>(params)
        .unwrap();
    serde_json::from_value(request.wait(WAIT).unwrap()).unwrap()
}

#[test]
fn file_change_during_startup_replays_latest_did_open() {
    for changes in [1, 2] {
        let f = notification_fixture();
        let pause = pause_first_spawn(&f);
        locked(&f, |manager| {
            manager.ensure_server_for_file_detailed(&f.file, &f.config)
        });
        pause.entered.recv_timeout(WAIT).unwrap();
        let mut expected = Vec::new();
        for version in 0..changes {
            let content = format!("content version {version}");
            std::fs::write(&f.file, &content).unwrap();
            expected = locked(&f, |manager| {
                manager.notify_file_changed_versioned(&f.file, &content, &f.config)
            })
            .unwrap();
        }
        pause.resume.send(()).unwrap();
        let ready =
            ensure_server_for_file_detailed_unlocked(|| f.manager.lock(), &f.file, &f.config);
        assert_eq!(ready.successful, vec![f.key.clone()]);
        let messages = recorded_notifications(&f);
        assert_eq!(
            messages.len(),
            1,
            "startup changes must coalesce into one didOpen: {messages:?}"
        );
        assert_eq!(messages[0]["method"], "textDocument/didOpen");
        assert_eq!(
            messages[0]["params"]["textDocument"]["text"],
            format!("content version {}", changes - 1)
        );
        assert_eq!(
            messages[0]["params"]["textDocument"]["version"],
            changes - 1
        );
        assert_eq!(expected, vec![(f.key.clone(), changes - 1)]);
    }
}

#[test]
fn file_change_during_cancelled_startup_is_not_replayed() {
    let f = notification_fixture();
    let pause = pause_first_spawn(&f);
    let (done_tx, done_rx) = unbounded();
    f.manager.lock().startup_finished_hook = Some(Arc::new(move || {
        done_tx.send(()).unwrap();
    }));
    locked(&f, |manager| {
        manager.ensure_server_for_file_detailed(&f.file, &f.config)
    });
    pause.entered.recv_timeout(WAIT).unwrap();
    let queued = locked(&f, |manager| {
        manager.notify_file_changed_versioned(&f.file, "never replay", &f.config)
    })
    .unwrap();
    assert_eq!(queued, vec![(f.key.clone(), 0)]);
    assert!(f.manager.lock().starting[&f.key]
        .pending_notifications
        .contains(&canonicalize_for_lsp(&f.file).unwrap()));
    f.manager.lock().quiesce();
    pause.resume.send(()).unwrap();
    done_rx.recv_timeout(WAIT).unwrap();
    f.manager.lock().resume();
    f.manager.lock().spawn_hook = None;
    f.manager.lock().startup_finished_hook = None;
    let ready = ensure_server_for_file_detailed_unlocked(|| f.manager.lock(), &f.file, &f.config);
    assert_eq!(ready.successful, vec![f.key.clone()]);
    assert!(
        recorded_notifications(&f).is_empty(),
        "closed reservation leaked a notification into its replacement"
    );
}

#[test]
fn unlocked_request_waits_until_startup_notification_replay_finishes() {
    let f = notification_fixture();
    let pause = pause_first_spawn(&f);
    locked(&f, |manager| {
        manager.ensure_server_for_file_detailed(&f.file, &f.config)
    });
    pause.entered.recv_timeout(WAIT).unwrap();
    std::fs::write(&f.file, "latest disk content").unwrap();
    locked(&f, |manager| {
        manager.notify_file_changed_versioned(&f.file, "not retained", &f.config)
    })
    .unwrap();
    let (entered_tx, entered_rx) = unbounded();
    let (resume_tx, resume_rx) = unbounded();
    let signal = Arc::clone(&f.manager.lock().starting[&f.key].signal);
    *signal.replay_hook.lock() = Some(Arc::new(move || {
        assert!(!MANAGER_LOCK_HELD.get());
        entered_tx.send(()).unwrap();
        resume_rx.recv_timeout(WAIT).unwrap();
    }));
    let (done_tx, done_rx) = unbounded();
    std::thread::scope(|scope| {
        let waiting = scope.spawn(|| {
            let result =
                ensure_server_for_file_detailed_unlocked(|| f.manager.lock(), &f.file, &f.config);
            done_tx.send(()).unwrap();
            result
        });
        let existing = scope.spawn(|| {
            let joined = wait_for_pending_startups_unlocked(|| f.manager.lock(), Some(&f.key.root));
            done_tx.send(()).unwrap();
            joined
        });
        pause.resume.send(()).unwrap();
        entered_rx.recv_timeout(WAIT).unwrap();
        let mut lock_available = false;
        let mut event_deferred = false;
        let mut readiness_blocked = false;
        if let Some(mut manager) = f.manager.try_lock_for(Duration::from_secs(5)) {
            lock_available = true;
            let client_hidden = manager.client_for_file_mut(&f.file, &f.config).is_none();
            let uri = uri_for_path(&canonicalize_for_lsp(&f.file).unwrap()).unwrap();
            let pull = manager.begin_open_document_pull(f.key.clone(), &f.file, &uri, None);
            readiness_blocked = client_hidden
                && manager.has_client(&f.key)
                && manager.active_server_keys().is_empty()
                && !manager.producer_has_settled(&f.key)
                && manager.navigation_requires_deferred_execution(&f.file, &f.config)
                && matches!(
                    pull.state,
                    DocumentPullState::Replied(Err(LspError::ServerNotReady(_)))
                )
                && manager.begin_workspace_pull(&f.key).is_err();
            let before = manager.starting[&f.key].deferred_events.len();
            let event = LspEvent::Notification {
                server_kind: f.key.kind.clone(),
                root: f.key.root.clone(),
                method: "custom/producerProgress".into(),
                params: None,
            };
            manager.event_tx.send(event.into()).unwrap();
            let drained = manager.drain_events_bounded(1);
            event_deferred = manager.clients.contains_key(&f.key)
                && manager.starting[&f.key].deferred_events.len() == before + 1
                && drained.events.is_empty()
                && drained.processed == 1;
        }
        let returned_early = done_rx.recv_timeout(Duration::from_secs(5)).is_ok();
        resume_tx.send(()).unwrap();
        assert_eq!(waiting.join().unwrap().successful, vec![f.key.clone()]);
        assert!(existing.join().unwrap());
        assert!(lock_available, "replay held the manager mutex");
        assert!(
            readiness_blocked,
            "uncommitted replay versions were exposed to requests"
        );
        assert!(
            event_deferred,
            "producer event passed uncommitted replay state"
        );
        assert!(!returned_early, "request returned before replay completed");
    });
    let delivered = f.manager.lock().drain_events_bounded(1);
    assert_eq!(delivered.processed, 1);
    assert!(
        matches!(&delivered.events[..], [LspEvent::Notification { method, .. }] if method == "custom/producerProgress")
    );
    assert!(f.manager.lock().drain_events().events.is_empty());
    let messages = recorded_notifications(&f);
    assert_eq!(
        messages[0]["params"]["textDocument"]["text"],
        "latest disk content"
    );
}

#[test]
fn startup_notification_path_bound_rejects_instead_of_dropping() {
    let f = notification_fixture();
    let pause = pause_first_spawn(&f);
    locked(&f, |manager| {
        manager.ensure_server_for_file_detailed(&f.file, &f.config)
    });
    pause.entered.recv_timeout(WAIT).unwrap();
    {
        let mut manager = f.manager.lock();
        let reservation = manager.starting.get_mut(&f.key).unwrap();
        reservation.notification_versions.insert(f.file.clone(), 0);
        for index in 1..STARTUP_NOTIFICATION_PATH_CAP {
            reservation
                .notification_versions
                .insert(f.file.with_file_name(format!("{index}.reserved")), 0);
        }
        assert_eq!(
            manager.queue_startup_notification(&f.key, &f.file).unwrap(),
            1
        );
        let extra = f.file.with_file_name("over-cap.reserved");
        let error = manager
            .queue_startup_notification(&f.key, &extra)
            .unwrap_err();
        assert_eq!(
            STARTUP_NOTIFICATION_QUEUE_FULL,
            "language-server startup notification queue is full; retry the file change"
        );
        assert!(
            matches!(error, LspError::ServerNotReady(ref reason) if reason == STARTUP_NOTIFICATION_QUEUE_FULL)
        );
        assert_eq!(
            manager.starting[&f.key].notification_versions.len(),
            STARTUP_NOTIFICATION_PATH_CAP
        );
        assert!(!manager.starting[&f.key]
            .pending_notifications
            .contains(&extra));
    }
    f.manager.lock().quiesce();
    pause.resume.send(()).unwrap();
}

#[test]
fn relative_diagnostics_wait_keeps_its_observation_budget_after_startup() {
    let f = notification_fixture();
    f.manager.lock().set_extra_env("AFT_STARTUP_PUSH", "1");
    let pause = pause_first_spawn(&f);
    locked(&f, |manager| {
        manager.notify_file_changed_versioned(&f.file, "content", &f.config)
    })
    .unwrap();
    pause.entered.recv_timeout(WAIT).unwrap();
    let (entered_tx, entered_rx) = unbounded();
    let (done_tx, done_rx) = unbounded();
    std::thread::scope(|scope| {
        let waiting = scope.spawn(|| {
            let mut manager = f.manager.lock();
            entered_tx.send(()).unwrap();
            let diagnostics =
                manager.wait_for_diagnostics(&f.file, &f.config, Duration::from_millis(250));
            done_tx.send(()).unwrap();
            diagnostics
        });
        entered_rx.recv_timeout(WAIT).unwrap();
        let returned_during_startup = done_rx.recv_timeout(Duration::from_secs(1)).is_ok();
        pause.resume.send(()).unwrap();
        let diagnostics = waiting.join().unwrap();
        assert!(
            !returned_during_startup,
            "startup consumed the observation timeout"
        );
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].message, "startup replay diagnostic");
    });
}

#[test]
fn watched_changes_during_startup_replay_latest_explicit_types_once() {
    let f = notification_fixture();
    f.manager.lock().set_extra_env("AFT_STARTUP_WATCHES", "1");
    let pause = pause_first_spawn(&f);
    locked(&f, |manager| {
        manager.notify_file_changed_versioned(&f.file, "content", &f.config)
    })
    .unwrap();
    pause.entered.recv_timeout(WAIT).unwrap();
    let existing = f.file.with_file_name("existing.json");
    let created = f.file.with_file_name("created.json");
    let deleted = f.file.with_file_name("deleted.json");
    for path in [&existing, &created] {
        std::fs::write(path, "{}").unwrap();
    }
    locked(&f, |manager| {
        manager.notify_files_watched_changed(
            &[
                (existing.clone(), FileChangeType::CHANGED),
                (created.clone(), FileChangeType::CREATED),
                (deleted.clone(), FileChangeType::CREATED),
            ],
            &f.config,
        )
    })
    .unwrap();
    locked(&f, |manager| {
        manager
            .notify_files_watched_changed(&[(deleted.clone(), FileChangeType::DELETED)], &f.config)
    })
    .unwrap();
    assert_eq!(
        f.manager.lock().pending_watched_changes[&f.key]
            .changes
            .len(),
        3
    );
    pause.resume.send(()).unwrap();
    let ready = ensure_server_for_file_detailed_unlocked(|| f.manager.lock(), &f.file, &f.config);
    assert_eq!(ready.successful, vec![f.key.clone()]);
    recorded_notifications(&f);
    f.manager.lock().drain_events();
    let messages = recorded_notifications(&f);
    let watched: Vec<_> = messages
        .iter()
        .filter(|message| message["method"] == "workspace/didChangeWatchedFiles")
        .collect();
    assert_eq!(watched.len(), 1, "{messages:?}");
    let changes = watched[0]["params"]["changes"].as_array().unwrap();
    assert_eq!(changes.len(), 3);
    for (name, kind) in [
        ("existing.json", 2),
        ("created.json", 1),
        ("deleted.json", 3),
    ] {
        let change = changes
            .iter()
            .find(|change| change["uri"].as_str().unwrap().ends_with(name))
            .unwrap();
        assert_eq!(change["type"], kind);
    }
    f.manager.lock().drain_events();
    assert_eq!(
        recorded_notifications(&f)
            .iter()
            .filter(|message| message["method"] == "workspace/didChangeWatchedFiles")
            .count(),
        1
    );
}

#[test]
fn watched_changes_wait_for_late_registration_without_restarting_server() {
    let f = notification_fixture();
    let ready = ensure_server_for_file_detailed_unlocked(|| f.manager.lock(), &f.file, &f.config);
    assert_eq!(ready.successful, vec![f.key.clone()]);
    let pid = f.manager.lock().clients[&f.key].child_pid();
    let path = f.file.with_file_name("late.json");
    std::fs::write(&path, "{}").unwrap();
    f.manager
        .lock()
        .notify_files_watched_changed(&[(path, FileChangeType::CREATED)], &f.config)
        .unwrap();
    assert!(recorded_notifications(&f).is_empty());
    recorded_notifications_with(&f, serde_json::json!({"register_watches": true}));
    f.manager.lock().drain_events();
    let messages = recorded_notifications(&f);
    assert_eq!(messages.len(), 1, "{messages:?}");
    assert_eq!(messages[0]["method"], "workspace/didChangeWatchedFiles");
    assert_eq!(messages[0]["params"]["changes"][0]["type"], 1);
    assert_eq!(f.manager.lock().clients[&f.key].child_pid(), pid);
}

#[test]
fn cancelled_startup_drops_queued_watched_changes() {
    let f = notification_fixture();
    f.manager.lock().set_extra_env("AFT_STARTUP_WATCHES", "1");
    let pause = pause_first_spawn(&f);
    locked(&f, |manager| {
        manager.ensure_server_for_file_detailed(&f.file, &f.config)
    });
    pause.entered.recv_timeout(WAIT).unwrap();
    let path = f.file.with_file_name("cancelled.json");
    std::fs::write(&path, "{}").unwrap();
    f.manager
        .lock()
        .notify_files_watched_changed(&[(path, FileChangeType::CHANGED)], &f.config)
        .unwrap();
    assert!(!f.manager.lock().pending_watched_changes.is_empty());
    f.manager.lock().quiesce();
    assert!(f.manager.lock().pending_watched_changes.is_empty());
    pause.resume.send(()).unwrap();
    f.manager.lock().resume();
    f.manager.lock().spawn_hook = None;
    let ready = ensure_server_for_file_detailed_unlocked(|| f.manager.lock(), &f.file, &f.config);
    assert_eq!(ready.successful, vec![f.key.clone()]);
    recorded_notifications(&f);
    f.manager.lock().drain_events();
    assert!(recorded_notifications(&f).is_empty());
}

#[test]
fn watcher_changes_during_startup_keep_kind_and_registration_filtering() {
    let f = notification_fixture();
    f.manager.lock().set_extra_env("AFT_STARTUP_WATCHES", "1");
    let pause = pause_first_spawn(&f);
    locked(&f, |manager| {
        manager.ensure_server_for_file_detailed(&f.file, &f.config)
    });
    pause.entered.recv_timeout(WAIT).unwrap();
    let path = f.file.with_file_name("watcher.json");
    std::fs::write(&path, "{}").unwrap();
    f.manager
        .lock()
        .forward_watcher_file_events(&[(path, FileChangeType::CREATED)], &[]);
    assert_eq!(
        f.manager.lock().pending_watched_changes[&f.key]
            .changes
            .len(),
        1
    );
    pause.resume.send(()).unwrap();
    let ready = ensure_server_for_file_detailed_unlocked(|| f.manager.lock(), &f.file, &f.config);
    assert_eq!(ready.successful, vec![f.key.clone()]);
    recorded_notifications(&f);
    f.manager.lock().drain_events();
    let messages = recorded_notifications(&f);
    assert_eq!(messages.len(), 1, "{messages:?}");
    assert_eq!(messages[0]["method"], "workspace/didChangeWatchedFiles");
    assert_eq!(messages[0]["params"]["changes"][0]["type"], 1);
}
