use super::*;

#[tokio::test]
async fn an_awaited_finish_without_a_pointer_does_not_wait_for_sqlite() {
    use crate::context::{App, AppContext};
    let dir = tempfile::tempdir().unwrap();
    let app = App::default_shared();
    let ctx = AppContext::from_app(
        app.clone(),
        crate::config::Config {
            project_root: Some(dir.path().into()),
            storage_dir: Some(dir.path().into()),
            ..Default::default()
        },
    );
    let registry = ctx.bash_background().clone();
    let key = registry.seed_awaited_task_for_test(dir.path(), dir.path());
    let task = registry.task(&key).unwrap();
    {
        let mut db = DeferredDbWrites::new(&registry, &task);
        let mut state = task.lock_state().unwrap();
        state.metadata.harness = Some("runner".into());
        state.metadata.awaited_work = Some(State::Finish {
            outcome: Outcome::Done,
            summary: "done".into(),
            notice: None,
        });
        registry
            .persist_task_locked(&task, &state.metadata, &mut db)
            .unwrap();
    }
    let pool = app.open_db(&dir.path().join("aft.db")).unwrap();
    registry.set_db_pool(pool.clone());
    let busy_store = pool.lock().unwrap();
    let saved = tokio::time::timeout(
        Duration::from_secs(1),
        tokio::task::spawn_blocking(move || registry.freeze_awaited_finish_notice(&task, None)),
    )
    .await;
    drop(busy_store);
    assert_eq!(
        saved
            .expect("card pool must not hold the finish")
            .unwrap()
            .unwrap(),
        None
    );
}

impl BgTaskRegistry {
    pub(crate) fn seed_awaited_opening_for_test(&self, key: &str) {
        self.set_awaited_marker_for_test(key, State::Opening);
    }

    pub(crate) fn seed_awaited_fallback_for_test(&self, key: &str) {
        self.set_awaited_marker_for_test(key, State::FallbackCancel);
    }

    pub(crate) fn set_awaited_cleanup_age_and_scope_for_test(
        &self,
        key: &str,
        age: Duration,
        scope: subc_protocol::scope::ScopeStamp,
    ) {
        let task = self.task(key).unwrap();
        let mut db = DeferredDbWrites::new(self, &task);
        let mut state = task.lock_state().unwrap();
        state.metadata.started_at = unix_millis().saturating_sub(age.as_millis() as u64);
        state.metadata.scope = Some(scope);
        self.persist_task_locked(&task, &state.metadata, &mut db)
            .unwrap();
    }

    fn set_awaited_marker_for_test(&self, key: &str, marker: State) {
        let task = self.task(key).unwrap();
        let mut db = DeferredDbWrites::new(self, &task);
        let mut state = task.lock_state().unwrap();
        state.metadata.awaited_work = Some(marker);
        self.persist_task_locked(&task, &state.metadata, &mut db)
            .unwrap();
    }

    pub(crate) fn drop_awaited_task_registration_for_test(&self, key: &str) {
        assert!(self.inner.tasks.lock().unwrap().remove(key).is_some());
    }
    pub(crate) fn seed_awaited_task_for_test(&self, storage: &Path, root: &Path) -> String {
        let key = "bash-0000000000000100";
        let layout =
            super::super::super::persistence::create_task_layout(storage, "shell-session", key)
                .unwrap();
        let paths = layout.paths.clone();
        let mut metadata = PersistedTask::starting(
            key.into(),
            "shell-session".into(),
            "command".into(),
            root.into(),
            Some(root.into()),
            None,
            true,
            true,
        );
        metadata.started_at = unix_millis().saturating_sub(1000);
        metadata.awaited_work = Some(State::Open);
        metadata.mark_terminal(BgTaskStatus::Completed, Some(0), None);
        super::super::super::persistence::write_task_at(&layout, &metadata).unwrap();
        self.insert_rehydrated_task(metadata, paths, true).unwrap();
        key.into()
    }

    pub(crate) fn attach_awaited_for_test(&self, service: &Arc<Service>) {
        self.attach_awaited_work(service);
    }

    pub(crate) fn save_awaited_finish_card_for_test(&self, key: &str, id: &str) {
        let task = self.task(key).unwrap();
        {
            let mut db = DeferredDbWrites::new(self, &task);
            let mut state = task.lock_state().unwrap();
            state.metadata.awaited_work = Some(State::Finish {
                outcome: Outcome::Done,
                summary: "saved summary".into(),
                notice: None,
            });
            self.persist_task_locked(&task, &state.metadata, &mut db)
                .unwrap();
        }
        assert_eq!(
            self.freeze_awaited_finish_notice(&task, Some(id.into()))
                .unwrap()
                .as_deref(),
            Some(id)
        );
    }

    pub(crate) fn publish_awaited_for_test(&self, key: &str) {
        let task = self.task(key).unwrap();
        let metadata = task.lock_state().unwrap().metadata.clone();
        self.enqueue_completion_if_needed(&metadata, Some(&task.paths), true);
    }

    pub(crate) fn race_awaited_for_test(&self, key: &str, read_terminal: impl FnOnce() + Send) {
        let task = self.task(key).unwrap();
        let lock = task.lock_state().unwrap();
        let barrier = std::sync::Barrier::new(3);
        std::thread::scope(|threads| {
            let ready = &barrier;
            let reader_task = &task;
            threads.spawn(move || {
                assert!(reader_task.state.try_lock().is_err());
                ready.wait();
                read_terminal();
            });
            threads.spawn(|| {
                assert!(task.state.try_lock().is_err());
                barrier.wait();
                self.publish_awaited_for_test(key);
            });
            barrier.wait();
            drop(lock);
        });
    }

    pub(crate) fn awaited_progress_for_test(&self, key: &str) {
        let task = self.task(key).unwrap();
        // A backdated task is due for a reminder if the awaited item isn't open.
        self.configure_long_running_reminders(true, 1);
        self.maybe_emit_long_running_reminder(&task);
        self.emit_bash_pattern_match(
            &task.session_id,
            PatternMatch {
                watch_id: "match-watch".into(),
                task_id: key.into(),
                match_text: "matched".into(),
                match_offset: 1,
                context: "one matched line".into(),
                once: true,
                batch: None,
                literal_pattern: false,
            },
        );
    }
}
