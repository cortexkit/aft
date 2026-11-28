mod tests {
    use crate::bash_background::persistence::{task_paths, PersistedTask};
    use crate::bash_background::watches::{PatternMatch, WatchPattern};
    use crate::bash_background::BgTaskStatus;
    use crate::protocol::BashPatternMatchFrame;
    use aft_engine::test_bridge::bash_remote::registry_support::registry_cases::*;
    use aft_engine::test_bridge::bash_remote::BgTaskRegistryTestExt as _;
    use std::time::Instant;

    #[test]
    fn recovered_terminal_reply_matches_live_status_text() {
        crate::subc::lower_hooks::register();
        use crate::commands::bash_orchestrate::{decide_bash_step, BashStep};
        for (status, exit, reason, expected) in [
            (BgTaskStatus::Failed, None, Some("spawn aborted"), "failed"),
            (BgTaskStatus::Failed, Some(17), None, "[exit code: 17]"),
            (BgTaskStatus::Killed, None, None, "killed"),
            (
                BgTaskStatus::Killed,
                None,
                Some("cancelled by caller"),
                "killed",
            ),
            (
                BgTaskStatus::TimedOut,
                None,
                Some("timeout elapsed"),
                "[command timed out]",
            ),
            (BgTaskStatus::Failed, Some(0x8000_0000), None, "2147483648"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let task_id = random_slug();
            let paths = task_paths(dir.path(), "session", &task_id).unwrap();
            let mut metadata = PersistedTask::starting(
                task_id,
                "session".into(),
                "unused".into(),
                dir.path().to_path_buf(),
                Some(dir.path().to_path_buf()),
                None,
                false,
                false,
            );
            metadata.mark_terminal(status.clone(), exit, reason.map(str::to_string));
            let row = metadata.to_bash_task_row("runner", &paths).unwrap();
            let snapshot = terminal_db_row_snapshot(row, metadata);
            let now = Instant::now();
            let BashStep::Done(live) =
                decide_bash_step(snapshot.clone(), now, true, false, now, "terminal")
            else {
                panic!("terminal record must have a live foreground result");
            };
            let context = crate::subc_format::FormatContext::from_tool_call(
                "bash",
                &serde_json::json!({}),
                dir.path(),
            );
            let live_text =
                crate::subc_format::format_response_with_context("bash", &live, &context);
            let key = crate::db::call_ledger::Key {
                carrier: "reserved:carrier".into(),
                call_key: "terminal".into(),
            };
            let (frame, _, retained_body) =
                aft_engine::test_bridge::bash_remote::recovered_bash_frame(
                    snapshot,
                    &key,
                    dir.path(),
                    None,
                )
                .expect("terminal recovered frame");
            let retained = retained_body
                .as_deref()
                .expect("recovered native result is retained");
            assert!(retained.len() < crate::db::call_ledger::MAX_TOOL_RESULT_SOURCE_BYTES);
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(retained).unwrap()["version"],
                1
            );
            let body: serde_json::Value = serde_json::from_slice(&frame.body).unwrap();
            assert_eq!(body["content"][0]["text"], live_text);
            assert!(
                live.success,
                "a failed command is still a successful tool call"
            );
            assert_eq!(body["isError"], false);
            assert!(
                live_text.contains(expected),
                "terminal {status:?}/{exit:?}/{reason:?} lost its status: {body}"
            );
            if let Some(reason) = reason {
                assert!(live_text.contains(reason), "{live_text}");
            }
        }
    }

    #[test]
    fn retry_overflow_sticky_gap_survives_restart_and_exact_ack() {
        let dir = tempfile::tempdir().unwrap();
        let (registry, task, _, _) = each_line_running_fixture(dir.path());
        let _reap = KillTasksOnDrop::new(registry.clone());
        let watch = registry
            .register_watch(
                task.task_id().to_owned(),
                WatchPattern::Substring("READY".into()),
                false,
            )
            .unwrap();
        let mut first = PatternMatch {
            durable_pending: false,
            task_id: task.task_id().to_owned(),
            watch_id: watch.clone(),
            match_text: "READY first".into(),
            match_offset: 10,
            context: "READY first".into(),
            once: false,
            batch: None,
            literal_pattern: false,
        };
        persist_watch_match(&registry, "session", task.task_id(), &mut first, 20, 0, 0);
        let mut second = first.clone();
        second.match_offset = 30;
        second.match_text = "READY second".into();
        persist_watch_match(&registry, "session", task.task_id(), &mut second, 40, 0, 0);
        let frame = BashPatternMatchFrame::new(
            task.task_id(),
            "session",
            &watch,
            "READY first",
            10,
            "READY first",
            false,
        );
        let candidate = crate::subc::watch_delivery::stalled_route_eviction(frame)
            .expect("stalled FIFO overflow must retain a watch-gap candidate");
        registry.retain_retry_gap("opencode", &candidate).unwrap();
        let pending = registry
            .pending_pattern_matches_for_session("session")
            .into_iter()
            .find(|f| f.watch_id == watch)
            .unwrap();
        let gap = pending.batch.as_ref().unwrap().retry_gap.as_ref().unwrap();
        assert_eq!(gap.reason, "retry_overflow");
        assert_eq!(gap.dropped_notifications, 1);
        assert_eq!(gap.known_missing_occurrences, 1);
        assert_eq!(gap.first_offset, Some(10));
        assert!(pending.context.contains("they cannot be recovered."));
        for terminal in [false, true] {
            ack_persisted_watches_for_task(&registry, "session", task.task_id(), terminal);
            assert!(registry
                .pending_pattern_matches_for_session("session")
                .iter()
                .any(|frame| frame.watch_id == watch));
        }
        assert!(registry
            .ack_pattern_match_for_session("session", task.task_id(), &watch, 30)
            .unwrap()
            .is_none());
        let (restarted, _, _) = registry_with_db_and_frames(dir.path());
        let restored = restarted
            .pending_pattern_matches_for_session("session")
            .into_iter()
            .find(|f| f.watch_id == watch)
            .unwrap();
        assert_eq!(restored.match_offset, pending.match_offset);
        assert_eq!(
            restored
                .batch
                .unwrap()
                .retry_gap
                .unwrap()
                .dropped_notifications,
            1
        );
        restarted
            .ack_pattern_match_for_session("session", task.task_id(), &watch, pending.match_offset)
            .unwrap()
            .unwrap();
        assert!(!restarted
            .pending_pattern_matches_for_session("session")
            .iter()
            .any(|f| f.watch_id == watch));
        // An evicted duplicate of the acknowledged frame must not reopen the gap.
        restarted.retain_retry_gap("opencode", &candidate).unwrap();
        assert!(!restarted
            .pending_pattern_matches_for_session("session")
            .iter()
            .any(|f| f.watch_id == watch));
        let later = BashPatternMatchFrame::new(
            task.task_id(),
            "session",
            &watch,
            "another lost notice",
            20,
            "another lost notice",
            false,
        );
        let later = crate::subc::watch_delivery::stalled_route_eviction(later).unwrap();
        restarted.retain_retry_gap("opencode", &later).unwrap();
        let newer = restarted
            .pending_pattern_matches_for_session("session")
            .into_iter()
            .find(|f| f.watch_id == watch)
            .unwrap();
        assert!(newer.match_offset > pending.match_offset);
        assert!(restarted
            .ack_pattern_match_for_session("session", task.task_id(), &watch, pending.match_offset)
            .unwrap()
            .is_none());
        restarted
            .ack_pattern_match_for_session("session", task.task_id(), &watch, newer.match_offset)
            .unwrap()
            .unwrap();
    }

    #[test]
    fn retry_overflow_gap_only_does_not_consume_once_watch_and_recoverable_batches_are_not_lost() {
        let dir = tempfile::tempdir().unwrap();
        let (registry, task, _, watch) = each_line_running_fixture_with_once(dir.path(), true);
        let _reap = KillTasksOnDrop::new(registry.clone());
        let frame = BashPatternMatchFrame::new(
            task.task_id(),
            "session",
            &watch,
            "unpersisted",
            10,
            "unpersisted",
            true,
        );
        let candidate = crate::subc::watch_delivery::stalled_route_eviction(frame).unwrap();
        registry.retain_retry_gap("opencode", &candidate).unwrap();
        let pending = registry
            .pending_pattern_matches_for_session("session")
            .pop()
            .unwrap();
        assert!(!pending.once);
        assert!(!pending.batch.as_ref().unwrap().has_matches());
        assert_eq!(
            pending
                .batch
                .as_ref()
                .unwrap()
                .retry_gap
                .as_ref()
                .unwrap()
                .known_missing_occurrences,
            0
        );
        registry
            .ack_pattern_match_for_session("session", task.task_id(), &watch, pending.match_offset)
            .unwrap()
            .unwrap();
        assert_eq!(registry.active_watch_count(task.task_id()), 1);
        append_each_line_output(&task.paths().stdout, b"READY after gap\n");
        registry.scan_task_watch_output(&task);
        let matched = registry
            .pending_pattern_matches_for_session("session")
            .pop()
            .unwrap();
        assert!(matched.once);
        assert!(matched.batch.as_ref().unwrap().retry_gap.is_none());
        let candidate =
            crate::subc::watch_delivery::stalled_route_eviction(matched.clone()).unwrap();
        registry.retain_retry_gap("opencode", &candidate).unwrap();
        let recovered = registry
            .pending_pattern_matches_for_session("session")
            .pop()
            .unwrap();
        assert_eq!(recovered.match_offset, matched.match_offset);
        assert!(recovered.batch.unwrap().retry_gap.is_none());
    }

    #[test]
    fn retry_overflow_retired_watch_gap_ack_prevents_replay_after_restart() {
        let dir = tempfile::tempdir().unwrap();
        let (registry, task, _, _) = each_line_running_fixture(dir.path());
        let _reap = KillTasksOnDrop::new(registry.clone());
        let frame = BashPatternMatchFrame::new(
            task.task_id(),
            "session",
            "retired-watch",
            "old match",
            10,
            "old match",
            false,
        );
        let candidate = crate::subc::watch_delivery::stalled_route_eviction(frame).unwrap();
        registry.retain_retry_gap("opencode", &candidate).unwrap();
        let notice = registry
            .pending_pattern_matches_for_session("session")
            .into_iter()
            .find(|f| f.watch_id == "retired-watch")
            .unwrap();
        assert!(!notice.once);
        assert!(notice.batch.as_ref().unwrap().lines.is_empty());
        registry
            .ack_pattern_match_for_session(
                "session",
                task.task_id(),
                "retired-watch",
                notice.match_offset,
            )
            .unwrap()
            .unwrap();
        let (restarted, _, _) = registry_with_db_and_frames(dir.path());
        restarted.retain_retry_gap("opencode", &candidate).unwrap();
        assert!(!restarted
            .pending_pattern_matches_for_session("session")
            .iter()
            .any(|f| f.watch_id == "retired-watch"));
    }

    #[test]
    fn retry_overflow_missing_task_keeps_a_session_gap_until_exact_ack() {
        let dir = tempfile::tempdir().unwrap();
        let (registry, _, _) = registry_with_db_and_frames(dir.path());
        let frame = BashPatternMatchFrame::new(
            "missing-task",
            "session",
            "missing-watch",
            "match",
            10,
            "match",
            false,
        );
        let candidate = crate::subc::watch_delivery::stalled_route_eviction(frame).unwrap();
        registry.retain_retry_gap("opencode", &candidate).unwrap();
        let (restarted, _, _) = registry_with_db_and_frames(dir.path());
        let notice = restarted
            .pending_pattern_matches_for_session("session")
            .pop()
            .unwrap();
        assert_eq!(notice.task_id, "missing-task");
        assert!(!notice.once);
        assert!(restarted.has_unacked_wakes_for_session("session"));
        assert!(restarted
            .unacked_wake_keys()
            .contains("match\0session\0missing-task\0missing-watch"));
        assert!(restarted
            .ack_pattern_match_for_session(
                "session",
                "missing-task",
                "missing-watch",
                notice.match_offset - 1
            )
            .unwrap()
            .is_none());
        restarted
            .ack_pattern_match_for_session(
                "session",
                "missing-task",
                "missing-watch",
                notice.match_offset,
            )
            .unwrap()
            .unwrap();
        assert!(restarted
            .pending_pattern_matches_for_session("session")
            .is_empty());
        assert!(!restarted.has_unacked_wakes_for_session("session"));
        restarted.retain_retry_gap("opencode", &candidate).unwrap();
        assert!(restarted
            .pending_pattern_matches_for_session("session")
            .is_empty());
    }
}
