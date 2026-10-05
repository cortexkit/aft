use aft::commands::read::handle_read;
use aft::config::Config;
use aft::context::{default_language_provider_factory, AppContext};
use aft::protocol::{RawRequest, Response};
use serde_json::{json, Value};
use std::{fs, path::Path};

fn read_response(root: &Path, file: &Path, extra: Value) -> Response {
    let ctx = AppContext::new(
        default_language_provider_factory(),
        crate::context_storage::isolate(Config {
            project_root: Some(root.to_path_buf()),
            ..Default::default()
        }),
    );
    let mut params = extra.as_object().cloned().unwrap();
    params.insert("file".into(), json!(file));
    handle_read(
        &RawRequest {
            id: "bounds".into(),
            command: "read".into(),
            lsp_hints: None,
            session_id: None,
            params: Value::Object(params),
        },
        &ctx,
    )
}

#[test]
fn ranged_huge_line_stops_at_response_budget() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("huge.txt");
    fs::write(&path, vec![b'x'; 2 * 1024 * 1024]).unwrap();
    let started = std::time::Instant::now();
    let response = read_response(temp.path(), &path, json!({"start_line": 1, "end_line": 1}));
    eprintln!(
        "ranged huge line: 2097152 source bytes, {:?}, complete={}",
        started.elapsed(),
        response.data["complete"]
    );
    assert_eq!(response.data["complete"], false);
    assert!(response.data.get("total_lines").is_none());
}

#[test]
fn limit_only_read_does_not_count_unread_tail() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("many.txt");
    fs::write(&path, "hello\n".repeat(350_000)).unwrap();
    let started = std::time::Instant::now();
    let response = read_response(temp.path(), &path, json!({"limit": 1}));
    eprintln!(
        "limit-only: 2100000 source bytes, {:?}, total_lines={}",
        started.elapsed(),
        response.data["total_lines"]
    );
    assert_eq!(response.data["lines_read"], 1);
    assert!(
        response.data.get("total_lines").is_none(),
        "unread total must remain unknown"
    );
}

#[test]
fn directory_listing_stops_enumeration() {
    let temp = tempfile::tempdir().unwrap();
    for n in 0..11_000 {
        fs::write(temp.path().join(format!("file-{n:05}")), "").unwrap();
    }
    let started = std::time::Instant::now();
    let response = read_response(temp.path(), temp.path(), json!({}));
    eprintln!(
        "directory: 11000 entries, {:?}, reported={}",
        started.elapsed(),
        response.data["total_entries"]
    );
    assert!(
        response.data["total_entries"].as_u64().unwrap() < 11_000,
        "must not exhaust the directory for a capped listing"
    );
    assert_eq!(response.data["complete"], false);
    let envelope = &response.data["entries_list_envelope"];
    assert_eq!(envelope["reason"], "walk");
    assert_eq!(envelope["total"]["kind"], "at_least");
    assert_eq!(envelope["shown"], 1000);
    assert_eq!(envelope["causes"], json!(["walk", "cap"]));
}

#[test]
fn wide_range_stops_scanning_when_output_is_full() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("wide.txt");
    fs::write(&path, format!("{}\n", "x".repeat(1000)).repeat(10_000)).unwrap();
    let response = read_response(
        temp.path(),
        &path,
        json!({"start_line": 1, "end_line": 10_000}),
    );
    assert_eq!(response.data["complete"], false);
    assert!(response.data["scan_bytes_examined"].as_u64().unwrap() < 60_000);
    assert!(response.data["content"]
        .as_str()
        .unwrap()
        .contains("narrow:"));
}

#[test]
fn small_streamed_range_preserves_crlf_and_unicode() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("small.txt");
    fs::write(&path, "hello\r\ncafé\r\n").unwrap();
    let response = read_response(temp.path(), &path, json!({"limit": 5}));
    assert_eq!(response.data["content"], "1: hello\n2: café\n");
    assert_eq!(response.data["total_lines"], 2);
    assert_eq!(response.data["complete"], true);
}

#[test]
fn concurrent_image_reads_report_decoder_capacity() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("large.png");
    image::DynamicImage::new_rgb8(2048, 2048)
        .save(&path)
        .unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
    let started = std::time::Instant::now();
    let responses = std::thread::scope(|scope| {
        let handles = (0..8)
            .map(|_| {
                let barrier = barrier.clone();
                let root = temp.path();
                let path = &path;
                scope.spawn(move || {
                    barrier.wait();
                    read_response(root, path, json!({}))
                })
            })
            .collect::<Vec<_>>();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>()
    });
    let busy = responses
        .iter()
        .filter(|response| {
            response.data["attachment_omitted_reason"]
                .as_str()
                .unwrap_or("")
                .contains("decoder capacity")
        })
        .count();
    eprintln!(
        "8 concurrent image reads: busy={busy}, elapsed={:?}",
        started.elapsed()
    );
    assert!(
        busy > 0,
        "concurrent decoders must have a finite admission capacity"
    );
}

#[test]
fn deep_range_skips_large_prefix_without_retaining_it() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("deep.log");
    let mut text = String::new();
    for line in 1..=60_000 {
        text.push_str(&format!("line-{line}:{}\n", "x".repeat(340)));
    }
    fs::write(&path, text).unwrap();
    let response = read_response(
        temp.path(),
        &path,
        json!({"start_line": 50_000, "end_line": 50_050}),
    );
    assert_eq!(response.data["lines_read"], 51);
    assert!(response.data["scan_gap"].is_null());
    assert!(response.data["content"]
        .as_str()
        .unwrap()
        .contains("50050: line-50050:"));
}

#[test]
fn range_after_huge_line_returns_requested_lines() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("minified.log");
    fs::write(
        &path,
        format!("{}\nsecond\nthird\nfourth\n", "x".repeat(2 * 1024 * 1024)),
    )
    .unwrap();
    let response = read_response(temp.path(), &path, json!({"start_line": 3, "end_line": 4}));
    assert_eq!(response.data["content"], "3: third\n4: fourth\n");
    assert_eq!(response.data["lines_read"], 2);
    assert!(response.data["scan_gap"].is_null());
}

#[test]
fn requested_huge_line_is_truncated_without_hiding_later_lines() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("minified.log");
    fs::write(
        &path,
        format!("{}\nsecond\nthird\n", "x".repeat(2 * 1024 * 1024)),
    )
    .unwrap();
    let response = read_response(temp.path(), &path, json!({"start_line": 1, "end_line": 3}));
    assert_eq!(response.data["lines_read"], 3);
    let text = response.data["content"].as_str().unwrap();
    assert!(text.contains("... (truncated)"));
    assert!(text.contains("2: second\n3: third\n"));
}

#[test]
fn directory_read_honors_sorted_windows() {
    let temp = tempfile::tempdir().unwrap();
    for n in (0..2620).rev() {
        fs::write(temp.path().join(format!("entry-{n:04}")), "").unwrap();
    }
    for (args, first, count) in [
        (json!({"limit": 10}), 0, 10),
        (json!({"offset": 11, "limit": 10}), 10, 10),
        (json!({"offset": "11", "limit": "10"}), 10, 10),
        (json!({"startLine": "11", "endLine": "20"}), 10, 10),
        (json!({"start_line": 11, "end_line": 20}), 10, 10),
        (json!({"startLine": 11, "offset": 21, "limit": 10}), 10, 10),
        (json!({}), 0, 1000),
        (json!({"limit": 2000}), 0, 1000),
    ] {
        let response = read_response(temp.path(), temp.path(), args.clone());
        let entries = response.data["entries"].as_array().unwrap();
        assert_eq!(entries.len(), count + 1, "window {args}");
        for (index, entry) in entries[..count].iter().enumerate() {
            assert_eq!(entry, &json!(format!("entry-{:04}", first + index)));
        }
        assert!(entries[count]
            .as_str()
            .unwrap()
            .contains(&format!("shown {count} of 2620 items")));
        assert!(entries[count]
            .as_str()
            .unwrap()
            .contains("narrow: path, offset, limit"));
        assert_eq!(response.data["complete"], false);
        assert_eq!(response.data["truncated"], true);
        assert_eq!(response.data["total_entries"], 2620);
        assert_eq!(response.data["total_entries_exact"], true);
    }
    let past = read_response(
        temp.path(),
        temp.path(),
        json!({"offset": 2621, "limit": 10}),
    );
    assert_eq!(past.data["entries"].as_array().unwrap().len(), 1);
    assert!(past.data["entries"][0]
        .as_str()
        .unwrap()
        .contains("offset 2621 exceeds 2620 entries"));
}
