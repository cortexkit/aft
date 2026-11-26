use super::*;

impl BgTaskRegistry {
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
        self.persist_task(&paths, &metadata).unwrap();
        self.insert_rehydrated_task(metadata, paths, true).unwrap();
        key.into()
    }

    pub(crate) fn attach_awaited_for_test(&self, service: &Arc<Service>) {
        self.attach_awaited_work(service);
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
