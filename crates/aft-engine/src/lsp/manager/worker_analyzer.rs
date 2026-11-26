use super::*;
use crate::lsp::client::{rust_analyzer_capacity, RustAnalyzerPermit};

const RECLAIM_IDLE_AGE: Duration = Duration::from_secs(2 * 60);
const CAPACITY_WAIT_LIMIT: Duration = Duration::from_secs(60);
// Exits notify waiters through the permit. Aging into eligibility, ending a
// check, and releasing a busy manager can also make a slot reclaimable.
const CAPACITY_RECHECK_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Default)]
pub(crate) struct DiagnosticsActivity {
    active: usize,
    last_finished: Option<Instant>,
}

pub(crate) struct DiagnosticsActivityGuard(Arc<parking_lot::Mutex<DiagnosticsActivity>>);

impl DiagnosticsActivityGuard {
    pub(crate) fn begin(activity: &Arc<parking_lot::Mutex<DiagnosticsActivity>>) -> Self {
        activity.lock().active += 1;
        Self(Arc::clone(activity))
    }
}

impl Drop for DiagnosticsActivityGuard {
    fn drop(&mut self) {
        let mut activity = self.0.lock();
        activity.active -= 1;
        activity.last_finished = Some(Instant::now());
    }
}

fn reclaim_priority(
    left: &(Instant, ServerKey),
    right: &(Instant, ServerKey),
) -> std::cmp::Ordering {
    left.0
        .cmp(&right.0)
        .then_with(|| server_key_sort(&left.1, &right.1))
}

impl LspManager {
    fn oldest_reclaimable_worker(
        &self,
        activity: &DiagnosticsActivity,
        now: Instant,
    ) -> Option<(Instant, ServerKey)> {
        if activity.active != 0 {
            return None;
        }
        self.clients
            .iter()
            .filter(|(key, client)| {
                key.kind == ServerKind::Rust
                    && !self.starting.contains_key(*key)
                    && client.can_reclaim_worker_analyzer()
            })
            .map(|(key, client)| {
                let last_used = client.last_used_at();
                (
                    last_used.max(activity.last_finished.unwrap_or(last_used)),
                    key.clone(),
                )
            })
            .filter(|(last_used, _)| now.saturating_duration_since(*last_used) >= RECLAIM_IDLE_AGE)
            .min_by(reclaim_priority)
    }
}

fn try_reclaim_worker(shared: &parking_lot::Mutex<ServerCapacity>, requester: &ServerKey) {
    let Some(mut shared) = shared.try_lock() else {
        return;
    };
    // A detached client retains its process permit until it exits. Do not
    // stop a second analyzer merely because the first is still shutting down.
    if shared.worker_reap.strong_count() != 0 || !rust_analyzer_capacity().is_full() {
        return;
    }
    let now = Instant::now();
    let mut oldest = None;
    for owner in &shared.managers {
        let Some(owner) = owner.upgrade() else {
            continue;
        };
        // Ordinary startup can hold a manager while taking the shared lock.
        // Never wait for a manager while holding that lock in reverse order.
        let Some(manager) = owner.try_lock() else {
            continue;
        };
        let activity = manager.diagnostics_activity.lock();
        if let Some(candidate) = manager.oldest_reclaimable_worker(&activity, now) {
            if oldest
                .as_ref()
                .is_none_or(|(best, _)| reclaim_priority(&candidate, best).is_lt())
            {
                oldest = Some((candidate, Arc::clone(&owner)));
            }
        }
    }
    let Some((candidate, owner)) = oldest else {
        return;
    };
    let Some(mut manager) = owner.try_lock() else {
        return;
    };
    let activity = Arc::clone(&manager.diagnostics_activity);
    let activity = activity.lock();
    if manager.oldest_reclaimable_worker(&activity, now).as_ref() != Some(&candidate)
        || !rust_analyzer_capacity().is_full()
    {
        return;
    }
    let (_, key) = candidate;
    let client = manager.detach_idle_client(&key);
    let lease = Arc::new(());
    shared.worker_reap = Arc::downgrade(&lease);
    drop(activity);
    drop(manager);
    drop(shared);
    slog_info!(
        "reclaimed idle worker rust-analyzer workspace={} for workspace={}",
        key.root.display(),
        requester.root.display()
    );
    LspManager::spawn_idle_lsp_reap_with_lease(vec![(key, client)], Some(lease));
}

fn reserve_worker_permit(
    shared: &parking_lot::Mutex<ServerCapacity>,
    server: &ServerKey,
    signal: &StartSignal,
    startup_deadline: Instant,
) -> Result<RustAnalyzerPermit, LspError> {
    let capacity = rust_analyzer_capacity();
    let deadline = startup_deadline.min(Instant::now() + CAPACITY_WAIT_LIMIT);
    let cancellation = crate::executor::current_job_cancellation();
    loop {
        if signal.cancelled.load(std::sync::atomic::Ordering::Acquire)
            || cancellation
                .as_ref()
                .is_some_and(|token| token.cancel_requested_before_commit())
        {
            return Err(LspError::ProjectClosed);
        }
        let failure = match capacity.try_acquire() {
            Ok(permit) => return Ok(permit),
            Err(failure) => failure,
        };
        if Instant::now() >= deadline {
            return Err(capacity.refuse(failure).into());
        }
        try_reclaim_worker(shared, server);
        capacity.wait_for_release(
            deadline
                .saturating_duration_since(Instant::now())
                .min(CAPACITY_RECHECK_INTERVAL),
        );
        if Instant::now() >= deadline {
            return Err(capacity.refuse(failure).into());
        }
    }
}

fn begin_inspect_start(
    manager: &Arc<parking_lot::Mutex<LspManager>>,
    candidate: &ApplicableServerCandidate,
    server: &ServerKey,
    config: &Config,
    deadline: Instant,
) -> (ServerAttemptResult, Option<Arc<StartSignal>>) {
    let (mut prepared, signal, capacity) = {
        let mut lsp = manager.lock();
        lsp.startup_owner = Arc::downgrade(manager);
        let start = lsp.begin_file_server_start(
            &candidate.definition,
            server,
            &candidate.source_file,
            config,
        );
        let signal = lsp.starting.get(server).map(|r| Arc::clone(&r.signal));
        match start {
            FileServerStart::Spawn(prepared)
                if server.kind == ServerKind::Rust && lsp.rust_cargo_disabled_for_worker() =>
            {
                (
                    prepared,
                    signal.expect("a prepared startup has a reservation"),
                    Arc::clone(&lsp.capacity),
                )
            }
            start => return (lsp.launch_file_server_start(server, start), signal),
        }
    };

    // Reserve before starting the child, but outside the manager lock so other
    // clients can finish their work or shut down while inspect waits.
    match reserve_worker_permit(&capacity, server, &signal, deadline) {
        Ok(permit) => {
            prepared.worker_permit = Some(permit);
            let lsp = manager.lock();
            if lsp
                .starting
                .get(server)
                .is_some_and(|r| Arc::ptr_eq(&r.signal, &signal))
            {
                lsp.launch_file_server_start(server, FileServerStart::Spawn(prepared));
            }
        }
        Err(error) => {
            let exit_log_line = match &error {
                LspError::Io(io) => format!(
                    "exited {:?} {} (spawn failed): pid=none phase={} status=never started elapsed=0.0s error={:?}",
                    server.kind, server.root.display(), ServerPhase::Spawn, io.to_string()
                ),
                _ => String::new(),
            };
            signal.complete(Err(SpawnFailure::NotStarted {
                error,
                exit_log_line,
            }));
        }
    }
    (
        ServerAttemptResult::InProgress {
            server_key: server.clone(),
        },
        Some(signal),
    )
}

/// Start or join one producer's reserved startup without holding the manager
/// mutex across process creation, initialization, or waiting for completion.
/// The request deadline limits waiting, not the worker's initialize handshake.
pub fn start_applicable_server_unlocked(
    manager: &Arc<parking_lot::Mutex<LspManager>>,
    snapshot: &ApplicableServerSnapshot,
    server: &ServerKey,
    config: &Config,
    deadline: Instant,
) -> ApplicableServerStartOutcomes {
    let mut outcomes = ApplicableServerStartOutcomes {
        failures: snapshot
            .producer_failures
            .iter()
            .filter(|f| f.server_key == *server)
            .cloned()
            .collect(),
        ..ApplicableServerStartOutcomes::default()
    };
    let Some(candidate) = snapshot.candidates.iter().find(|c| c.key == *server) else {
        return outcomes;
    };
    let mut retried_abandoned = false;
    let result = loop {
        if Instant::now() >= deadline {
            outcomes.deadline_exceeded = Some(server.clone());
            return outcomes;
        }
        let (initial, signal) = begin_inspect_start(manager, candidate, server, config, deadline);
        let abandoned_before_join = signal.as_ref().is_some_and(|signal| {
            signal
                .request_deadline_exceeded
                .load(std::sync::atomic::Ordering::Acquire)
        });
        let result = if let (ServerAttemptResult::InProgress { .. }, Some(signal)) =
            (&initial, signal)
        {
            wait_for_startup_outcome(|| manager.lock(), &signal, deadline).unwrap_or_else(|| {
                signal
                    .request_deadline_exceeded
                    .store(true, std::sync::atomic::Ordering::Release);
                initial
            })
        } else {
            initial
        };
        // A new request can overlap cleanup of an earlier timed-out request.
        // Observe its result, then retry once with the new request's budget and
        // configuration instead of returning that abandoned attempt's failure.
        if abandoned_before_join
            && !retried_abandoned
            && matches!(
                result,
                ServerAttemptResult::SpawnFailed { .. }
                    | ServerAttemptResult::BinaryNotInstalled { .. }
            )
            && Instant::now() < deadline
        {
            let lsp = manager.lock();
            if !lsp.is_quiesced() && lsp.failure_to_replay(server).is_none() {
                retried_abandoned = true;
                continue;
            }
        }
        break result;
    };
    match result {
        ServerAttemptResult::Ok { server_key } => outcomes.successful.push(server_key),
        ServerAttemptResult::InProgress { server_key } => {
            outcomes.in_progress.push(server_key.clone());
            outcomes.deadline_exceeded = Some(server_key);
        }
        result => outcomes.failures.push(ApplicableServerFailure {
            server_key: server.clone(),
            result,
        }),
    }
    outcomes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn analyzer(
        root: PathBuf,
        shared: &Arc<parking_lot::Mutex<ServerCapacity>>,
        worker: bool,
    ) -> (Arc<parking_lot::Mutex<LspManager>>, ServerKey) {
        std::fs::create_dir(&root).unwrap();
        let key = ServerKey {
            kind: ServerKind::Rust,
            root,
        };
        let manager = Arc::new(parking_lot::Mutex::new(LspManager::new()));
        let mut locked = manager.lock();
        locked.set_server_capacity(Arc::clone(shared));
        locked.enable_background_document_pulls(Arc::downgrade(&manager));
        let binary = crate::fake_lsp_test_helper::fake_server_binary();
        locked.override_binary(ServerKind::Rust, binary.clone());
        if worker {
            locked.disable_rust_cargo_for_worker();
        }
        let (events, _receiver) = crossbeam_channel::unbounded();
        let mut client = LspClient::spawn_with_reclaim_root(
            ServerKind::Rust,
            key.root.clone(),
            &binary,
            &[],
            &HashMap::from([("AFT_FAKE_LSP_HOLD_HOVER".into(), "1".into())]),
            LspEventSender::Standalone(events),
            locked.child_registry.clone(),
            None,
            worker,
        )
        .expect("spawn fake rust-analyzer");
        client.initialize(&key.root, None).unwrap();
        locked.clients.insert(key.clone(), client);
        drop(locked);
        (manager, key)
    }

    fn aged(manager: &parking_lot::Mutex<LspManager>, key: &ServerKey, at: Instant) {
        manager
            .lock()
            .clients
            .get_mut(key)
            .unwrap()
            .set_last_used_at_for_test(at);
    }

    #[test]
    fn rust_worker_inspect_capacity_reclaims_only_idle_lru_and_preserves_busy_workers() {
        let tmp = tempfile::tempdir().unwrap();
        let root = crate::path_identity::canonicalize_normalized(tmp.path());
        let shared = Arc::new(parking_lot::Mutex::new(ServerCapacity::default()));
        let (first, first_key) = analyzer(root.join("first-worker"), &shared, true);
        let (second, second_key) = analyzer(root.join("second-worker"), &shared, true);
        let (head, head_key) = analyzer(root.join("head"), &shared, false);
        let request_key = ServerKey {
            kind: ServerKind::Rust,
            root: root.join("request"),
        };
        let hover = first.lock().clients.get_mut(&first_key).unwrap().start_request::<lsp_types::request::HoverRequest>(
            serde_json::from_value(serde_json::json!({
                "textDocument": {
                    "uri": url::Url::from_file_path(first_key.root.join("lib.rs")).unwrap().as_str(),
                },
                "position": { "line": 0, "character": 0 },
            })).unwrap(),
        ).unwrap();
        second
            .lock()
            .clients
            .get_mut(&second_key)
            .unwrap()
            .record_rust_progress(
                "rust-analyzer/flycheck/0",
                "begin",
                Some("cargo check"),
                None,
            );
        let now = Instant::now();
        aged(
            &first,
            &first_key,
            now - RECLAIM_IDLE_AGE - Duration::from_secs(10),
        );
        aged(
            &second,
            &second_key,
            now - RECLAIM_IDLE_AGE - Duration::from_secs(5),
        );
        aged(
            &head,
            &head_key,
            now - RECLAIM_IDLE_AGE - Duration::from_secs(20),
        );

        let signal = StartSignal::default();
        let deadline = Instant::now() + Duration::from_millis(120);
        let result = reserve_worker_permit(&shared, &request_key, &signal, deadline);
        assert!(
            Instant::now() >= deadline,
            "busy analyzers must not cause immediate refusal"
        );
        assert!(
            matches!(result, Err(LspError::Io(error)) if error.to_string().contains("rust_analyzer_capacity_exhausted"))
        );
        assert!(
            first.lock().clients.contains_key(&first_key),
            "pending request protects its analyzer"
        );
        assert!(
            second.lock().clients.contains_key(&second_key),
            "flycheck protects its analyzer"
        );

        assert!(hover.wait(Duration::ZERO).is_err());
        second
            .lock()
            .clients
            .get_mut(&second_key)
            .unwrap()
            .record_rust_progress("rust-analyzer/flycheck/0", "end", None, None);

        let activity = Arc::clone(&first.lock().diagnostics_activity);
        let guard = DiagnosticsActivityGuard::begin(&activity);
        assert!(first
            .lock()
            .oldest_reclaimable_worker(&activity.lock(), Instant::now())
            .is_none());
        drop(guard);
        let last_finished = activity.lock().last_finished.unwrap();
        assert!(first
            .lock()
            .oldest_reclaimable_worker(
                &activity.lock(),
                last_finished + RECLAIM_IDLE_AGE - Duration::from_millis(1)
            )
            .is_none());
        assert!(first
            .lock()
            .oldest_reclaimable_worker(&activity.lock(), last_finished + RECLAIM_IDLE_AGE)
            .is_some());
        activity.lock().last_finished = Some(now - RECLAIM_IDLE_AGE - Duration::from_secs(10));

        let permit = reserve_worker_permit(
            &shared,
            &request_key,
            &signal,
            Instant::now() + Duration::from_secs(5),
        )
        .expect("the oldest idle worker should release a slot");
        assert!(
            !first.lock().clients.contains_key(&first_key),
            "oldest idle worker should be detached"
        );
        assert!(
            second.lock().clients.contains_key(&second_key),
            "only one worker should be reaped"
        );
        assert!(
            head.lock().clients.contains_key(&head_key),
            "head is never a reclaim candidate"
        );
        assert!(
            rust_analyzer_capacity().is_full(),
            "new permit still counts toward the cap"
        );
        drop(permit);

        let file = first_key.root.join("lib.rs");
        std::fs::write(&file, "fn main() {}\n").unwrap();
        std::fs::write(
            first_key.root.join("Cargo.toml"),
            "[package]\nname = 'first-worker'\nversion = '0.1.0'\n[lib]\npath = 'lib.rs'\n",
        )
        .unwrap();
        let config = Config::default();
        let walk = walk_applicable_area(&first_key.root, Some(&[file]), &config, None).unwrap();
        let snapshot = first.lock().classify_applicable_servers(walk, &config);
        let outcome = start_applicable_server_unlocked(
            &first,
            &snapshot,
            &first_key,
            &config,
            Instant::now() + Duration::from_secs(5),
        );
        assert_eq!(
            outcome.successful,
            vec![first_key],
            "reclaimed worker restarts on its next inspect"
        );
    }
}
