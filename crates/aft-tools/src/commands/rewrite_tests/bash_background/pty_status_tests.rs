//! `bash_status` on a PTY task reads only what it needs from the capture: the
//! screen is kept up to date from bytes written since the last poll, and raw
//! output without `output_offset` is the capture's last page.

use crate::bash_background::persistence::{write_task, BgMode};
use crate::bash_background::registry::ARTIFACT_PAGE_BYTES;
use aft_engine::test_bridge::bash_remote::{
    registry_support::registry_cases::{
        full_artifact_read_calls, insert_terminal_piped_task, reset_full_artifact_read_calls,
    },
    BgBuffer, BgTask, TaskRuntime, TaskRuntimeTestExt as _,
};
use std::{fs, sync::Arc};

const PAGE: usize = ARTIFACT_PAGE_BYTES;

fn context(dir: &tempfile::TempDir) -> crate::context::AppContext {
    crate::context::AppContext::from_app(
        crate::test_app(),
        crate::config::Config {
            project_root: Some(dir.path().into()),
            storage_dir: Some(dir.path().into()),
            ..Default::default()
        },
    )
}

/// A finished PTY task whose capture is `capture`.
fn finished_pty_task(
    ctx: &crate::context::AppContext,
    dir: &tempfile::TempDir,
    capture: &[u8],
) -> (String, Arc<BgTask>) {
    let (id, task) = insert_terminal_piped_task(ctx.bash_background(), dir, "true", "", "", false);
    {
        let mut state = task.test_lock_state().unwrap();
        state.metadata_mut().mode = BgMode::Pty;
        *state.runtime_mut() = TaskRuntime::empty_pty();
        *state.buffer_mut() = BgBuffer::registered(task.paths(), BgMode::Pty);
        write_task(&task.paths().json, state.metadata()).unwrap();
    }
    fs::write(&task.paths().pty, capture).unwrap();
    (id, task)
}

fn status(
    ctx: &crate::context::AppContext,
    id: &str,
    output_mode: &str,
) -> crate::protocol::Response {
    let request = serde_json::from_value(serde_json::json!({
        "id": "pty-status",
        "command": "bash_status",
        "session_id": "session",
        "params": {"task_id": id, "output_mode": output_mode},
    }))
    .unwrap();
    crate::commands::bash_status::handle(&request, ctx)
}

#[test]
fn pty_screen_polls_parse_only_new_capture_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(&dir);
    let mut capture = b"first line\r\n".repeat(3 * PAGE / 12);
    let (id, task) = finished_pty_task(&ctx, &dir, &capture);
    reset_full_artifact_read_calls();

    let first = status(&ctx, &id, "screen");
    assert!(first.success, "status failed: {:?}", first.data);
    assert!(first.data["pty_screen"]
        .as_str()
        .unwrap()
        .ends_with("first line"));
    capture.extend_from_slice(b"second line\r\n");
    fs::write(&task.paths().pty, &capture).unwrap();
    let second = status(&ctx, &id, "screen");

    assert!(second.success, "status failed: {:?}", second.data);
    assert!(
        second.data["pty_screen"]
            .as_str()
            .unwrap()
            .ends_with("first line\nsecond line"),
        "{:?}",
        second.data["pty_screen"]
    );
    assert_eq!(
        full_artifact_read_calls(),
        0,
        "a screen poll read the whole PTY capture"
    );
}

#[test]
fn pty_raw_status_without_offset_returns_the_last_page() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(&dir);
    let mut capture = vec![b'x'; 2 * PAGE];
    capture.extend(vec![b'y'; PAGE]);
    let (id, _task) = finished_pty_task(&ctx, &dir, &capture);
    reset_full_artifact_read_calls();

    let response = status(&ctx, &id, "raw");

    assert!(response.success, "status failed: {:?}", response.data);
    let marker = format!(
        "[PTY raw output: showing the last {PAGE} of {} bytes; read earlier bytes with output_offset.]\n",
        3 * PAGE
    );
    let raw = response.data["pty_raw"].as_str().unwrap();
    assert_eq!(raw, format!("{marker}{}", "y".repeat(PAGE)));
    assert_eq!(response.data["pty_raw_omitted_bytes"], 2 * PAGE);
    assert_eq!(full_artifact_read_calls(), 0);
}

#[test]
fn pty_raw_status_returns_a_capture_within_one_page_whole() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(&dir);
    let (id, _task) = finished_pty_task(&ctx, &dir, b"short output");

    let response = status(&ctx, &id, "raw");

    assert!(response.success, "status failed: {:?}", response.data);
    assert_eq!(response.data["pty_raw"], "short output");
    assert!(response.data.get("pty_raw_omitted_bytes").is_none());
}
