use crate::bash_background::persistence::{read_task_at, resolve_task_layout};
use crate::harness::Harness;
use aft_engine::test_bridge::bash_remote::registry_support::{
    cleanup_finished_at, enqueue_counted_completion, watchdog_fixture as fixture,
};
use aft_engine::test_bridge::bash_remote::BgTaskRegistryTestExt as _;
use std::time::{Duration, Instant};

#[test]
fn watchdog_cost_status_poll_preserves_session_completion_delivery() {
    let storage = tempfile::tempdir().unwrap();
    for tool_response in [false, true] {
        for queued_before_poll in [false, true] {
            let ctx = crate::test_context(
                crate::context::default_language_provider_factory(),
                crate::config::Config::default(),
            );
            let registry = ctx.bash_background();
            let (metadata, paths) = fixture(storage.path(), "origin", false, 0);
            registry
                .insert_rehydrated_task(metadata.clone(), paths.clone(), true)
                .unwrap();
            if queued_before_poll {
                enqueue_counted_completion(registry, &metadata, &paths);
            }
            let data = serde_json::json!({"task_id": paths.task_id, "status": "completed", "exit_code": 0, "output_preview": "done"});
            let mut status = crate::protocol::Response::success("status", data.clone());
            if tool_response {
                crate::response_finalize::finalize_tool_response(
                    &mut status,
                    &mut String::new(),
                    &ctx,
                    "origin",
                    "bash_status",
                    true,
                );
            } else {
                crate::response_finalize::finalize_response_with_bg_completions(
                    &mut status,
                    &ctx,
                    "origin",
                    "bash_status",
                    true,
                );
            }
            assert!(status.data.get("bg_completions").is_none());
            assert!(
                !read_task_at(&resolve_task_layout(&paths.session_dir, &paths.task_id).unwrap())
                    .unwrap()
                    .completion_delivered,
                "a read-only terminal status poll acknowledged an undelivered completion"
            );
            let retention = Duration::from_secs(24 * 60 * 60);
            let cleanup_at = Instant::now() + retention + Duration::from_secs(1);
            cleanup_finished_at(registry, retention, cleanup_at);
            assert!(paths.json.exists(), "cleanup removed an undelivered result");
            // Terminal rendering can enqueue after a status snapshot is returned.
            if !queued_before_poll {
                enqueue_counted_completion(registry, &metadata, &paths);
            }
            let mut foreign = crate::protocol::Response::success("foreign", serde_json::json!({}));
            crate::response_finalize::finalize_response_with_bg_completions(
                &mut foreign,
                &ctx,
                "other",
                "echo",
                true,
            );
            assert!(foreign.data.get("bg_completions").is_none());
            let mut origin = crate::protocol::Response::success("origin", serde_json::json!({}));
            crate::response_finalize::finalize_response_with_bg_completions(
                &mut origin,
                &ctx,
                "origin",
                "echo",
                true,
            );
            assert_eq!(origin.data["bg_completions"][0]["task_id"], paths.task_id);
            let mut watch = crate::protocol::Response::success("watch", data);
            crate::response_finalize::finalize_response_with_bg_completions(
                &mut watch,
                &ctx,
                "origin",
                "bash_watch",
                true,
            );
            assert!(registry
                .drain_completions_for_session(Some("origin"))
                .is_empty());
            assert!(
                read_task_at(&resolve_task_layout(&paths.session_dir, &paths.task_id).unwrap())
                    .unwrap()
                    .completion_delivered
            );
            cleanup_finished_at(registry, retention, cleanup_at);
            assert!(
                !paths.dir.exists(),
                "cleanup retained a collected result past 24 hours"
            );
        }
    }
}

#[test]
fn watchdog_cost_terminal_reply_acknowledges_only_originating_session() {
    let storage = tempfile::tempdir().unwrap();
    let ctx = crate::test_context(
        crate::context::default_language_provider_factory(),
        crate::config::Config::default(),
    );
    let registry = ctx.bash_background();
    for harness in [
        Harness::Runner,
        Harness::Opencode,
        Harness::Pi,
        Harness::Mcp {
            client: "fixture".into(),
        },
    ] {
        registry.set_harness(harness);
        for command in ["bash_watch", "bash"] {
            let (metadata, paths) = fixture(storage.path(), "origin", false, 0);
            registry
                .insert_rehydrated_task(metadata, paths.clone(), true)
                .unwrap();
            let data = serde_json::json!({"task_id": paths.task_id, "status": "completed", "exit_code": 0, "output_preview": "done"});
            let mut foreign = crate::protocol::Response::success("foreign", data.clone());
            crate::response_finalize::finalize_response_with_bg_completions(
                &mut foreign,
                &ctx,
                "other",
                command,
                false,
            );
            assert!(
                !read_task_at(&resolve_task_layout(&paths.session_dir, &paths.task_id).unwrap())
                    .unwrap()
                    .completion_delivered
            );
            let mut text = "done".to_string();
            let mut reply = crate::protocol::Response::success("reply", data);
            crate::response_finalize::finalize_tool_response(
                &mut reply, &mut text, &ctx, "origin", command, false,
            );
            assert!(
                read_task_at(&resolve_task_layout(&paths.session_dir, &paths.task_id).unwrap())
                    .unwrap()
                    .completion_delivered,
                "{command} returned a terminal result but left it undelivered"
            );
        }
    }
}
