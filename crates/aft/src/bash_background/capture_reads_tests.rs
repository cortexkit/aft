use std::ffi::OsStr;

use super::*;
use crate::bash_background::registry::BgTaskRegistry;
use crate::log_redact::SECRET_PLACEHOLDER;

pub(crate) fn terminal_task(
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

pub(crate) fn with_secret(task: &super::super::super::BgTask, value: &str) {
    *task.completion_secrets.lock().unwrap() = Some(SecretValues::from_environment([(
        OsStr::new("API_TOKEN"),
        OsStr::new(value),
    )]));
}

/// Lines of `filler` text, `bytes` long in total.
pub(crate) fn filler(bytes: usize) -> String {
    let line = "compiling crate foo v0.1.0 (/src/foo)\n";
    let mut text = line.repeat(bytes / line.len());
    text.push_str(&"x".repeat(bytes - text.len()));
    text
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

pub(crate) const DISPLAY_LINE_FINISH_BYTES: u64 = LINE_FINISH_BYTES;
