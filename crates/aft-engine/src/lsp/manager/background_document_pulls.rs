use super::*;
use std::collections::BTreeMap;

const CLOSED_PULL_DEBOUNCE: Duration = Duration::from_millis(500);
const CLOSED_PULL_ROUND_FILES: usize = 64;
const CLOSED_PULL_COOLDOWN: Duration = Duration::from_secs(10);

#[derive(Default)]
pub(super) struct ClosedDocumentPulls {
    pending: HashMap<ServerKey, ClosedPullRound>,
    recent: HashMap<(ServerKey, PathBuf), ClosedPullStamp>,
}

struct ClosedPullRound {
    instance: ServerInstanceId,
    due: Instant,
    // True after the first failure. A new push can start another attempt.
    files: BTreeMap<PathBuf, bool>,
}

struct ClosedPullStamp {
    until: Instant,
    input_generation: u64,
    report_epoch: u64,
}

type ClosedBatch = (ServerKey, ServerInstanceId, Vec<(PathBuf, bool)>);

impl ClosedDocumentPulls {
    pub(super) fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    pub(super) fn retain_servers(&mut self, keep: impl Fn(&ServerKey) -> bool) {
        self.pending.retain(|key, _| keep(key));
        self.recent.retain(|(key, _), _| keep(key));
    }

    fn round(
        &mut self,
        key: &ServerKey,
        instance: ServerInstanceId,
        due: Instant,
    ) -> &mut ClosedPullRound {
        self.pending
            .entry(key.clone())
            .or_insert_with(|| ClosedPullRound {
                instance,
                due,
                files: BTreeMap::new(),
            })
    }

    fn next_due(&self) -> Option<Instant> {
        self.pending.values().map(|round| round.due).min()
    }

    fn take_due(&mut self, now: Instant) -> Option<ClosedBatch> {
        let (key, round) = self
            .pending
            .iter_mut()
            .filter(|(_, round)| round.due <= now)
            .min_by_key(|(_, round)| round.due)?;
        let key = key.clone();
        let instance = round.instance;
        let mut files = Vec::with_capacity(round.files.len().min(CLOSED_PULL_ROUND_FILES));
        while files.len() < CLOSED_PULL_ROUND_FILES {
            let Some(file) = round.files.pop_first() else {
                break;
            };
            files.push(file);
        }
        if round.files.is_empty() {
            self.pending.remove(&key);
        }
        Some((key, instance, files))
    }

    fn remember(&mut self, key: &ServerKey, file: &Path, stamp: ClosedPullStamp, now: Instant) {
        self.recent.retain(|_, stamp| stamp.until > now);
        self.recent.insert((key.clone(), file.to_path_buf()), stamp);
    }
}

impl LspManager {
    /// A temporary recertification close can itself cause gopls to republish a
    /// versionless report. Do not start the same round again on an exact
    /// duplicate if neither this file nor the server's other inputs changed.
    pub(super) fn discard_duplicate_closed_push(
        &self,
        key: &ServerKey,
        file: &Path,
        diagnostics: &[StoredDiagnostic],
        now: Instant,
    ) -> bool {
        let Some(stamp) = self
            .closed_document_pulls
            .recent
            .get(&(key.clone(), file.to_path_buf()))
        else {
            return false;
        };
        if now >= stamp.until
            || self
                .documents
                .get(key)
                .is_some_and(|store| store.is_open(file))
            || !self.clients.get(key).is_some_and(|client| {
                client.diagnostic_input_generation() == stamp.input_generation
            })
        {
            return false;
        }
        let duplicate = self
            .diagnostics
            .report_for_server_file(key, file)
            .is_some_and(|entry| {
                entry.epoch == stamp.report_epoch
                    && !entry.provisional
                    && entry.diagnostics == diagnostics
                    && self.report_matches_current_content(key, file, entry)
            });
        if duplicate {
            slog_info!(
                "lsp_protocol server={} root={} method=textDocument/publishDiagnostics event=uncertified-because-duplicate-after-recertification file={}",
                key.kind.id_str(), key.root.display(), file.display()
            );
        }
        duplicate
    }

    pub(super) fn queue_publish_document_pull(&mut self, key: &ServerKey, file: &Path) {
        if self
            .documents
            .get(key)
            .is_some_and(|store| store.is_open(file))
        {
            self.queue_background_document_pull(key, file);
            return;
        }
        if self.is_quiesced()
            || self.starting.contains_key(key)
            || self.background_pull_owner.strong_count() == 0
        {
            return;
        }
        let Some(client) = self.clients.get(key).filter(|client| {
            client.state() == ServerState::Ready && client.uses_document_pull(Some(file))
        }) else {
            return;
        };
        if !self.needs_closed_pull(key, file) {
            return;
        }
        let due = Instant::now() + CLOSED_PULL_DEBOUNCE;
        let round = self.closed_document_pulls.round(key, client.instance, due);
        round.due = due;
        round.files.insert(file.to_path_buf(), false);
        self.start_background_document_pull_worker();
    }

    fn needs_closed_pull(&self, key: &ServerKey, file: &Path) -> bool {
        self.diagnostics
            .report_for_server_file(key, file)
            .is_some_and(DiagnosticEntry::needs_recertification)
    }

    fn same_closed_pull_server(&self, key: &ServerKey, instance: ServerInstanceId) -> bool {
        !self.is_quiesced()
            && self
                .clients
                .get(key)
                .is_some_and(|client| client.instance == instance)
    }

    fn begin_closed_pull(
        &mut self,
        key: &ServerKey,
        file: &Path,
        instance: ServerInstanceId,
    ) -> Result<(DocumentPull, u64), LspError> {
        let opened = self.open_document_for_servers(file, std::slice::from_ref(key))?;
        let input = self.clients[key].diagnostic_input_generation();
        let result = self.begin_document_pull(key, file, Instant::now() + Self::PULL_FILE_TIMEOUT);
        match result {
            Ok(mut pull) => {
                if !opened.is_empty() {
                    pull.opened_for_pull = pull.document_version.map(|version| (instance, version));
                }
                Ok((pull, input))
            }
            Err(error) => {
                let _ = self.close_inspect_documents(file, &opened);
                Err(error)
            }
        }
    }

    fn finish_closed_pull(&mut self, pull: DocumentPull, input: u64) {
        let key = pull.key.clone();
        let file = pull.canonical_path.clone();
        let opened_for_pull = pull.opened_for_pull;
        let outcome = self.finish_document_pull(pull);
        let successful = matches!(
            &outcome,
            PullFileOutcome::Full { .. } | PullFileOutcome::Unchanged
        );
        let uninterrupted = self
            .clients
            .get(&key)
            .is_some_and(|client| client.diagnostic_input_generation() == input);
        self.close_documents_opened_for_pulls(
            &file,
            &[PullFileResult {
                server_key: key.clone(),
                outcome,
                opened_for_pull,
            }],
        );
        if !successful || !uninterrupted || opened_for_pull.is_none() {
            return;
        }
        let Some(client) = self.clients.get(&key) else {
            return;
        };
        if client.diagnostic_input_generation() != input.wrapping_add(1) {
            return;
        }
        if let Some(entry) = self.diagnostics.report_for_server_file(&key, &file) {
            let now = Instant::now();
            self.closed_document_pulls.remember(
                &key,
                &file,
                ClosedPullStamp {
                    until: now + CLOSED_PULL_COOLDOWN,
                    input_generation: client.diagnostic_input_generation(),
                    report_epoch: entry.epoch,
                },
                now,
            );
        }
    }

    fn background_document_pull_finished(&mut self) {
        self.background_pull_inflight = false;
        for waiter in std::mem::take(&mut self.background_pull_idle_waiters) {
            let _ = waiter.send(());
        }
    }
}

pub(super) fn run_background_document_pulls(owner: Weak<parking_lot::Mutex<LspManager>>) {
    let Some(manager) = owner.upgrade() else {
        return;
    };
    let _worker = DocumentPullWorker(owner);
    loop {
        let mut state = manager.lock();
        if state.is_quiesced() || state.background_pull_pauses > 0 {
            return;
        }
        if !state.background_pull_pending.is_empty() {
            let batch = std::mem::take(&mut state.background_pull_pending);
            drop(state);
            run_open_batch(&manager, batch);
        } else if let Some(batch) = state.closed_document_pulls.take_due(Instant::now()) {
            drop(state);
            run_closed_batch(&manager, batch);
        } else if let Some(due) = state.closed_document_pulls.next_due() {
            drop(state);
            std::thread::sleep(due.saturating_duration_since(Instant::now()));
        } else {
            return;
        }
    }
}

fn run_open_batch(manager: &parking_lot::Mutex<LspManager>, batch: HashSet<(ServerKey, PathBuf)>) {
    // Finish a batch before taking newly coalesced input, so a continuously
    // edited file cannot starve other open files in the same manager.
    let mut batch = batch.into_iter();
    while let Some((key, file)) = batch.next() {
        let pull = {
            let mut manager = manager.lock();
            if manager.is_quiesced() {
                return;
            }
            if manager.background_pull_pauses > 0 {
                manager.background_pull_pending.insert((key, file));
                manager.background_pull_pending.extend(batch);
                return;
            }
            let eligible = manager
                .clients
                .get(&key)
                .is_some_and(|client| client.uses_document_pull(Some(&file)))
                && manager
                    .documents
                    .get(&key)
                    .is_some_and(|store| store.is_open(&file));
            if !eligible {
                continue;
            }
            match manager.begin_document_pull(
                &key,
                &file,
                Instant::now() + LspManager::PULL_FILE_TIMEOUT,
            ) {
                Ok(pull) => {
                    manager.background_pull_inflight = true;
                    pull
                }
                Err(_) => continue,
            }
        };
        let pull = pull.wait();
        let mut manager = manager.lock();
        manager.finish_document_pull(pull);
        manager.background_document_pull_finished();
    }
}

fn run_closed_batch(manager: &parking_lot::Mutex<LspManager>, (key, instance, files): ClosedBatch) {
    let mut files = files.into_iter();
    while let Some((file, retry)) = files.next() {
        let mut state = manager.lock();
        if !state.same_closed_pull_server(&key, instance) {
            return;
        }
        if state.background_pull_pauses > 0 {
            let round = state
                .closed_document_pulls
                .round(&key, instance, Instant::now());
            for (file, retry) in std::iter::once((file, retry)).chain(files) {
                round.files.entry(file).or_insert(retry);
            }
            return;
        }
        if !state.needs_closed_pull(&key, &file)
            || !state.clients[&key].uses_document_pull(Some(&file))
        {
            continue;
        }
        match state.begin_closed_pull(&key, &file, instance) {
            Ok((pull, input)) => {
                state.background_pull_inflight = true;
                slog_info!("lsp_protocol server={} root={} method=textDocument/diagnostic event=recertifying-closed-file file={} retry={retry}", key.kind.id_str(), key.root.display(), file.display());
                drop(state);
                // A slow language server must not block other manager users.
                let pull = pull.wait();
                state = manager.lock();
                if state.same_closed_pull_server(&key, instance) {
                    state.finish_closed_pull(pull, input);
                }
                state.background_document_pull_finished();
            }
            Err(error) => slog_info!(
                "could not recertify closed document {}: {error}",
                file.display()
            ),
        }
        if !retry
            && state.same_closed_pull_server(&key, instance)
            && state.needs_closed_pull(&key, &file)
        {
            let due = Instant::now() + CLOSED_PULL_DEBOUNCE;
            state
                .closed_document_pulls
                .round(&key, instance, due)
                .files
                .entry(file)
                .or_insert(true);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Fixture {
        _dir: tempfile::TempDir,
        manager: Arc<parking_lot::Mutex<LspManager>>,
        key: ServerKey,
        file: PathBuf,
    }

    impl Fixture {
        fn after_first_pull() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let root = canonicalize_for_lsp(dir.path()).unwrap();
            let file = root.join("broken.go");
            std::fs::write(root.join("go.mod"), "module example.com/repro\n").unwrap();
            std::fs::write(&file, "package main\n").unwrap();
            let key = ServerKey {
                kind: ServerKind::Go,
                root: root.clone(),
            };
            let config = Config {
                project_root: Some(root),
                ..Config::default()
            };
            let manager = Arc::new(parking_lot::Mutex::new(LspManager::new()));
            {
                let mut state = manager.lock();
                state.override_binary(
                    ServerKind::Go,
                    crate::fake_lsp_test_helper::fake_server_binary(),
                );
                state.set_extra_env("AFT_FAKE_LSP_PULL", "1");
            }
            let reports = pull_file_diagnostics_unlocked_inner(
                || manager.lock(),
                &file,
                &config,
                None,
                false,
            )
            .unwrap();
            assert!(matches!(
                reports[0].outcome,
                PullFileOutcome::Full {
                    diagnostic_count: 1
                }
            ));
            manager
                .lock()
                .close_documents_opened_for_pulls(&file, &reports);
            Self {
                _dir: dir,
                manager,
                key,
                file,
            }
        }

        fn push(&self, message: &str) -> Option<PathBuf> {
            self.manager.lock().handle_publish_diagnostics(
                self.key.kind.clone(),
                self.key.root.clone(),
                &json!({
                    "uri": uri_for_path(&self.file).unwrap(),
                    "diagnostics": [{
                        "range": {"start":{"line":4,"character":0},"end":{"line":4,"character":8}},
                        "severity": 1, "code": "E0PULL", "source": "fake-lsp", "message": message
                    }]
                }),
            )
        }

        fn recertified() -> Self {
            let fixture = Self::after_first_pull();
            assert!(fixture.push("trigger recertification").is_some());
            let instance = fixture.manager.lock().clients[&fixture.key].instance;
            run_closed_batch(
                &fixture.manager,
                (
                    fixture.key.clone(),
                    instance,
                    vec![(fixture.file.clone(), false)],
                ),
            );
            let state = fixture.manager.lock();
            assert!(!state.documents[&fixture.key].is_open(&fixture.file));
            assert!(state
                .closed_document_pulls
                .recent
                .contains_key(&(fixture.key.clone(), fixture.file.clone())));
            drop(state);
            fixture
        }

        fn assert_stored(&self, message: &str) {
            assert_eq!(self.push(message), Some(self.file.clone()));
            let state = self.manager.lock();
            let entry = state
                .diagnostics
                .report_for_server_file(&self.key, &self.file)
                .unwrap();
            assert!(entry.needs_recertification());
            assert_eq!(entry.diagnostics[0].message, message);
        }
    }

    const PULLED_MESSAGE: &str = "test pull diagnostic";

    #[test]
    fn equivalent_self_push_does_not_restart_closed_document_pull_during_cooldown() {
        let fixture = Fixture::recertified();
        let mut state = fixture.manager.lock();
        state.enable_background_document_pulls(Arc::downgrade(&fixture.manager));
        let original_epoch = state.diagnostics.current_epoch();
        drop(state);
        assert!(fixture.push(PULLED_MESSAGE).is_none());
        let mut state = fixture.manager.lock();
        assert_eq!(state.diagnostics.current_epoch(), original_epoch);
        assert!(state.closed_document_pulls.is_empty());
        state
            .closed_document_pulls
            .recent
            .get_mut(&(fixture.key.clone(), fixture.file.clone()))
            .unwrap()
            .until = Instant::now();
        drop(state);
        fixture.assert_stored(PULLED_MESSAGE);
    }

    #[test]
    fn changed_diagnostics_are_stored_during_closed_document_pull_cooldown() {
        Fixture::recertified().assert_stored("new diagnostic from a dependency");
    }

    #[test]
    fn push_after_other_workspace_input_is_stored_during_closed_document_pull_cooldown() {
        let fixture = Fixture::recertified();
        let other = fixture.key.root.join("clean.go");
        let text = "package main\n";
        std::fs::write(&other, text).unwrap();
        fixture
            .manager
            .lock()
            .notify_file_changed_for_server_keys(other, text, vec![fixture.key.clone()])
            .unwrap();
        fixture.assert_stored(PULLED_MESSAGE);
    }

    #[test]
    fn push_after_disk_drift_is_stored_during_closed_document_pull_cooldown() {
        let fixture = Fixture::recertified();
        std::fs::write(&fixture.file, "package main\n// unsent edit\n").unwrap();
        fixture.assert_stored(PULLED_MESSAGE);
    }

    #[test]
    fn quiesce_cancels_the_pending_closed_document_pull_round() {
        let fixture = Fixture::after_first_pull();
        let mut state = fixture.manager.lock();
        state.enable_background_document_pulls(Arc::downgrade(&fixture.manager));
        let pause = state.pause_background_document_pulls();
        drop(state);
        assert!(fixture.push("trigger recertification").is_some());
        let mut state = fixture.manager.lock();
        assert!(!state.closed_document_pulls.is_empty());
        let clients = state.quiesce();
        assert!(state.closed_document_pulls.is_empty());
        drop(state);
        drop(pause);
        assert!(!fixture.manager.lock().background_pull_running);
        LspManager::spawn_idle_lsp_reap(clients);
    }
}
