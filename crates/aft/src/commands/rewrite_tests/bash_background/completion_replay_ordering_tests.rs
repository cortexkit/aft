use crate::bash_background::persistence::{read_task_at, resolve_task_layout};
use aft_engine::test_bridge::bash_remote::registry_support::ordering::*;
use aft_engine::test_bridge::bash_remote::successful_spawn_response;
use std::{fs, sync::Arc, time::Duration};

#[test]
fn terminal_background_launch_preserves_completion_before_and_after_publication() {
    for publish_before_reply in [false, true] {
        for pty in [false, true] {
            let storage = tempfile::tempdir().unwrap();
            let ctx = crate::test_context(
                crate::context::default_language_provider_factory(),
                crate::config::Config::default(),
            );
            let owner = ctx.bash_background();
            let frames = capture_live_ordering_frames(owner);
            let task = live_secret_task(owner, storage.path());
            let (reached_rx, release_tx) = pause_ordering_publication(&task);
            fs::write(&task.paths().exit, "0").unwrap();
            let publisher = {
                let owner = owner.clone();
                let task = Arc::clone(&task);
                std::thread::spawn(move || {
                    aft_engine::test_bridge::bash_remote::poll_task(&owner, &task)
                })
            };
            reached_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            let mut publisher = Some(publisher);
            if publish_before_reply {
                release_tx.send(()).unwrap();
                publisher.take().unwrap().join().unwrap().unwrap();
            }

            // Force the child-exit-before-launch-reply ordering. This reply
            // carries launch guidance, not the finished command's output.
            let request = serde_json::from_value(serde_json::json!({
                "id": "launch", "command": "bash", "session_id": SESSION,
                "params": {"command": "printf output", "background": !pty, "pty": pty},
            }))
            .unwrap();
            let spawn = successful_spawn_response("launch", SESSION, task.task_id(), pty, &ctx);
            assert_eq!(spawn.data["status"], "completed");
            let crate::response_finalize::DispatchOutcome::Immediate(mut reply) =
                crate::commands::bash_orchestrate::build_bash_outcome(&request, &ctx, spawn)
            else {
                panic!("background launch must not wait");
            };
            crate::response_finalize::finalize_response(&mut reply, &ctx, SESSION, "bash");
            let delivered = read_task_at(
                &resolve_task_layout(&task.paths().session_dir, task.task_id()).unwrap(),
            )
            .unwrap()
            .completion_delivered;
            if let Some(publisher) = publisher {
                release_tx.send(()).unwrap();
                publisher.join().unwrap().unwrap();
            }

            assert!(
                !delivered,
                "launch guidance acknowledged unseen terminal output (publish_before_reply={publish_before_reply}, pty={pty})"
            );
            let live = completion_frames(&frames, task.task_id());
            assert_eq!(live.len(), 1);
            assert_eq!(live[0]["output_preview"], format!("finished {MASKED}\n"));
            let queued = owner.drain_completions_for_session(Some(SESSION));
            assert_eq!(queued.len(), 1);
            assert_eq!(queued[0].task_id, task.task_id());

            let (restored, replay_frames) = registry(storage.path());
            restored.replay_session(storage.path(), SESSION).unwrap();
            let replayed = restored.drain_completions_for_session(Some(SESSION));
            assert_eq!(replayed.len(), 1);
            assert_eq!(replayed[0].output_preview, queued[0].output_preview);
            assert!(completion_frames(&replay_frames, task.task_id()).is_empty());
        }
    }
}
