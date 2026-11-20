use std::ffi::OsStr;

use super::*;
use crate::bash_background::persistence::work_counts;
use crate::bash_background::registry::BgTaskRegistry;
use crate::log_redact::SECRET_PLACEHOLDER;

fn terminal_task(
    registry: &BgTaskRegistry,
    fixture: &tempfile::TempDir,
    stdout: &str,
    compressed: bool,
) -> std::sync::Arc<super::super::super::BgTask> {
    let (_, task) = super::super::super::tests::insert_terminal_piped_task(
        registry, fixture, "cmd", stdout, "", compressed,
    );
    task
}

fn with_secret(task: &super::super::super::BgTask, value: &str) {
    *task.completion_secrets.lock().unwrap() = Some(SecretValues::from_environment([(
        OsStr::new("API_TOKEN"),
        OsStr::new(value),
    )]));
}

fn display_detail(
    registry: &BgTaskRegistry,
    task: &std::sync::Arc<super::super::super::BgTask>,
) -> serde_json::Value {
    let (metadata, buffer) = {
        let state = task.lock_state().unwrap();
        (state.metadata.clone(), state.buffer.clone())
    };
    registry.display_bodies(task, &metadata, &buffer).detail
}

/// Lines of `filler` text, `bytes` long in total.
fn filler(bytes: usize) -> String {
    let line = "compiling crate foo v0.1.0 (/src/foo)\n";
    let mut text = line.repeat(bytes / line.len());
    text.push_str(&"x".repeat(bytes - text.len()));
    text
}

fn install_tail_display(metadata: &mut crate::bash_background::persistence::PersistedTask) {
    use crate::bash_background::frames::{Identity, TaskDisplay};

    crate::subc::lower_hooks::register();
    let pin = crate::subc::tool_views::SHIPPED
        .keys()
        .find(|pin| crate::subc::tool_views::has_bash_tail(pin))
        .unwrap();
    metadata.display = Some(TaskDisplay {
        identity: Identity {
            runner: subc_protocol::Principal::Reserved {
                module_id: "broca".into(),
            },
            call_key: "capture-display".into(),
            schema_pin: pin.clone(),
        },
        capture_key: "capture-display".into(),
        reply_produced: false,
        deferred: false,
        held_ms: None,
        attempted: false,
    });
}

#[test]
fn display_detail_reads_only_the_lines_its_limit_can_show() {
    let fixture = tempfile::tempdir().unwrap();
    let registry = BgTaskRegistry::default();
    let stdout = filler(64 * 1024 * 1024);
    let task = terminal_task(&registry, &fixture, &stdout, false);
    let limit = crate::subc::rich_ui::display::detail_byte_limit();
    let (mut metadata, buffer) = {
        let state = task.lock_state().unwrap();
        (state.metadata.clone(), state.buffer.clone())
    };
    install_tail_display(&mut metadata);

    work_counts::reset();
    let bodies = registry.display_bodies(&task, &metadata, &buffer);
    let read = work_counts::get().artifact_bytes as u64;

    assert!(
        (limit..=limit + LINE_FINISH_BYTES).contains(&read),
        "the detail read {read} capture bytes to show at most {limit}"
    );
    assert!(bodies.tail.is_none(), "a detail prefix is not a tail");
    let detail = bodies.detail;
    let shown = detail["stdout"].as_str().unwrap();
    assert!(shown.len() as u64 >= limit);
    assert!(stdout.starts_with(shown) && shown.ends_with('\n'));
    assert_eq!(detail["truncated"], true);
    assert_eq!(detail["omittedBytes"], stdout.len() - shown.len());
}

#[test]
fn display_detail_includes_tail_when_its_bounded_read_reaches_the_end() {
    let fixture = tempfile::tempdir().unwrap();
    let registry = BgTaskRegistry::default();
    let limit = crate::subc::rich_ui::display::detail_byte_limit() as usize;
    // The complete capture fits in the read's line-finishing allowance but
    // not in the display detail's byte budget.
    let stdout = format!("{}\nfirst\nsecond\nthird\nlast\n", filler(limit));
    let task = terminal_task(&registry, &fixture, &stdout, false);
    let (mut metadata, buffer) = {
        let state = task.lock_state().unwrap();
        (state.metadata.clone(), state.buffer.clone())
    };
    install_tail_display(&mut metadata);

    work_counts::reset();
    let bodies = registry.display_bodies(&task, &metadata, &buffer);

    assert_eq!(work_counts::get().artifact_bytes as usize, stdout.len());
    assert_eq!(
        bodies.tail,
        Some(serde_json::json!([
            {"text": "second"},
            {"text": "third"},
            {"text": "last"}
        ]))
    );
}

#[test]
fn display_detail_masks_a_secret_on_the_line_crossing_its_limit() {
    let fixture = tempfile::tempdir().unwrap();
    let registry = BgTaskRegistry::default();
    let limit = crate::subc::rich_ui::display::detail_byte_limit() as usize;
    let secret = "s3cr3t-display-value-0123456789";
    let mut stdout = filler(limit - 10);
    stdout.push_str(&format!("token {secret} end\n"));
    stdout.push_str(&filler(2 * LINE_FINISH_BYTES as usize));
    let task = terminal_task(&registry, &fixture, &stdout, false);
    with_secret(&task, secret);

    let detail = display_detail(&registry, &task);
    let shown = detail["stdout"].as_str().unwrap();
    assert!(!shown.contains(&secret[..8]), "secret prefix shown");
    assert!(shown.contains(SECRET_PLACEHOLDER));
    assert_eq!(detail["truncated"], true);
}

#[test]
fn display_detail_reads_whole_when_a_json_secret_is_open_at_its_cut() {
    let fixture = tempfile::tempdir().unwrap();
    let registry = BgTaskRegistry::default();
    let limit = crate::subc::rich_ui::display::detail_byte_limit() as usize;
    let mut stdout = filler(limit - 10);
    // The value holds a raw newline just past the limit, so a cut there
    // would leave its first half unmasked.
    stdout.push_str("{\"password\": \"first-half-of-value\nsecond-half\"}\n");
    stdout.push_str(&filler(2 * LINE_FINISH_BYTES as usize));
    let task = terminal_task(&registry, &fixture, &stdout, false);

    let detail = display_detail(&registry, &task);
    let shown = detail["stdout"].as_str().unwrap();
    assert!(
        !shown.contains("first-half-of-value"),
        "open JSON secret shown"
    );
}

fn notice_preview(
    registry: &BgTaskRegistry,
    task: &std::sync::Arc<super::super::super::BgTask>,
) -> String {
    let cache = registry.ensure_terminal_output_cache(task).unwrap();
    let metadata = task.lock_state().unwrap().metadata.clone();
    registry
        .completion_preview_for_notice(&metadata, &cache, Some(&task.paths))
        .0
}

#[test]
fn notice_preview_masks_secrets_that_straddle_scan_chunks() {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            set_scan_chunk_for_test(None);
        }
    }
    let _reset = Reset;
    set_scan_chunk_for_test(Some(64));
    let secret = "s3cr3t-notice-value-0123456789";
    // Previews show the end of the output, so both credentials sit near it.
    // The exact value starts 57 bytes into a 64-byte read and so crosses it.
    let base = filler(4000);
    let pad = (57 + 64 - (base.len() + 7) % 64) % 64;
    let exact = format!("{base}{}\nvalue={secret}\ndone\n", "x".repeat(pad));
    assert_eq!(exact.find(secret).unwrap() % 64, 57);
    // A JSON secret field whose label and value sit on different lines.
    let json = format!("{base}\n{{\"password\":\n\"json-secret-value\"}}\ndone\n");
    for compressed in [false, true] {
        for (stdout, hidden) in [(&exact, secret), (&json, "json-secret-value")] {
            let fixture = tempfile::tempdir().unwrap();
            let registry = BgTaskRegistry::default();
            let task = terminal_task(&registry, &fixture, stdout, compressed);
            with_secret(&task, secret);
            let preview = notice_preview(&registry, &task);
            assert!(
                !preview.contains(hidden),
                "compressed={compressed}: {hidden} shown in {preview}"
            );
            assert!(preview.contains(SECRET_PLACEHOLDER), "{preview}");
        }
    }
}

#[test]
fn notice_scan_finds_clean_captures_clean_and_every_secret_window() {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            set_scan_chunk_for_test(None);
        }
    }
    let _reset = Reset;
    let fixture = tempfile::tempdir().unwrap();
    let registry = BgTaskRegistry::default();
    let clean = filler(10_000);
    let task = terminal_task(&registry, &fixture, &clean, false);
    let buffer = task.lock_state().unwrap().buffer.clone();
    let secret = "s3cr3t-scan-value-0123456789";
    with_secret(&task, secret);
    for chunk in [7, 64, 4096, SCAN_CHUNK_BYTES] {
        set_scan_chunk_for_test(Some(chunk));
        let secrets = task.completion_secrets.lock().unwrap();
        let secrets = secrets.as_ref().unwrap();
        assert_eq!(capture_needs_full_mask(&buffer, secrets), Some(false));
        // Every placement of the value, including across each read edge.
        for at in (0..clean.len()).step_by(997) {
            let mut text = clean.clone();
            text.insert_str(at, secret);
            std::fs::write(&task.paths.stdout, &text).unwrap();
            assert_eq!(
                capture_needs_full_mask(&buffer, secrets),
                Some(true),
                "chunk {chunk}, value at {at}"
            );
        }
        std::fs::write(&task.paths.stdout, &clean).unwrap();
    }
}

#[test]
fn raw_edges_match_the_whole_normalized_capture() {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            set_scan_chunk_for_test(None);
        }
    }
    let _reset = Reset;
    let fixture = tempfile::tempdir().unwrap();
    let registry = BgTaskRegistry::default();
    // Carriage returns exercise normalization; `é` exercises char boundaries.
    let stdout = format!("{}progress 10%\rprogress 99%\né-start\n", filler(9_000));
    let (_, task) = super::super::super::tests::insert_terminal_piped_task(
        &registry,
        &fixture,
        "cmd",
        &stdout,
        "stderr line é\nlast",
        false,
    );
    let buffer = task.lock_state().unwrap().buffer.clone();
    let mut whole = crate::bash_background::buffer::combine_streams(&stdout, "stderr line é\nlast");
    super::super::super::normalize_piped_display_output(&mut whole);
    for chunk in [5, 64, 1000, SCAN_CHUNK_BYTES] {
        set_scan_chunk_for_test(Some(chunk));
        for width in [1, 33, 500, 8_999, 20_000] {
            let expected = super::super::RawSlices::of(&whole, width);
            let edges = read_raw_edges(&buffer, width).unwrap();
            let actual = match &edges {
                RawEdges::Whole(text) => super::super::RawSlices::of(text, width),
                RawEdges::Ends { head, tail } => super::super::RawSlices {
                    head,
                    more_after_head: true,
                    tail,
                    more_before_tail: true,
                },
            };
            assert_eq!(actual.head, expected.head, "chunk {chunk} width {width}");
            assert_eq!(actual.tail, expected.tail, "chunk {chunk} width {width}");
            assert_eq!(actual.more_after_head, expected.more_after_head);
            assert_eq!(actual.more_before_tail, expected.more_before_tail);
        }
    }
}
