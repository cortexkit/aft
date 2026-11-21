//! URL fetches against local test servers: deadlines for servers that stall
//! in one phase, remembered failures, and reuse of the document cache. Most
//! tests run the fetch under a short call budget, the same way a tool call
//! does, and check the typed timeout and how long it took.

#[cfg(unix)]
use std::io::Write;
use std::thread;
#[cfg(unix)]
use std::time::Duration;
use std::time::Instant;

#[cfg(unix)]
use crate::url_fetch::cache_content_path_for_url;

#[allow(unused_imports)]
pub(crate) use aft_base::url_fetch::deadline_tests::*;

#[test]
fn outline_reports_the_typed_timeout_and_its_phase() {
    let storage = tempfile::tempdir().unwrap();
    let address = serve(|_, _stream| thread::sleep(SERVER_STALL));
    let ctx = crate::test_context(
        Box::new(crate::parser::TreeSitterProvider::new()),
        crate::config::Config {
            url_fetch_allow_private: true,
            storage_dir: Some(storage.path().to_path_buf()),
            ..Default::default()
        },
    );
    let req = serde_json::from_value(serde_json::json!({
        "id": "silent-url",
        "command": "outline",
        "file": url(address, "/silent.md"),
    }))
    .unwrap();
    let _call = crate::bounded_io::enter(Some(crate::bounded_io::Budget::after(CALL_BUDGET)));
    let started = Instant::now();
    let response = crate::commands::outline::handle_outline(&req, &ctx);
    assert!(!response.success);
    assert_eq!(
        response.data["code"], "url_fetch_timeout",
        "{:?}",
        response.data
    );
    assert_eq!(response.data["phase"], "headers", "{:?}", response.data);
    assert!(started.elapsed() < FETCH_DONE_BY);
}

#[test]
fn outline_says_a_repeated_failure_is_cached_and_when_it_is_fetched_again() {
    let storage = tempfile::tempdir().unwrap();
    let (address, _) = status_server();
    let ctx = crate::test_context(
        Box::new(crate::parser::TreeSitterProvider::new()),
        crate::config::Config {
            url_fetch_allow_private: true,
            storage_dir: Some(storage.path().to_path_buf()),
            ..Default::default()
        },
    );
    let req: crate::protocol::RawRequest = serde_json::from_value(serde_json::json!({
        "id": "failing-url",
        "command": "outline",
        "file": url(address, "/503"),
    }))
    .unwrap();
    let first = crate::commands::outline::handle_outline(&req, &ctx);
    assert_eq!(first.data["code"], "url_fetch_failed", "{:?}", first.data);
    assert!(first.data.get("cached").is_none(), "{:?}", first.data);

    let second = crate::commands::outline::handle_outline(&req, &ctx);
    assert_eq!(second.data["code"], "url_fetch_failed", "{:?}", second.data);
    assert_eq!(second.data["cached"], true, "{:?}", second.data);
    assert!(
        second.data["retry_after_ms"].as_u64().unwrap() <= 60_000,
        "{:?}",
        second.data
    );
    assert!(second.data["message"]
        .as_str()
        .unwrap()
        .contains("cached failure"));
}

#[cfg(unix)]
#[test]
fn a_fetch_ending_inside_its_deadline_and_a_stalled_read_back_still_reply_before_the_call_budget() {
    let storage = tempfile::tempdir().unwrap();
    // A 6 s call: the fetch deadline is 3 s, and this server answers at
    // 2.6 s, so the download ends just inside it.
    let budget = Duration::from_secs(6);
    let answer_after = Duration::from_millis(2_600);
    let address = serve(move |_, mut stream| {
        thread::sleep(answer_after);
        let body = b"# Late\n\n## Section\n\ntext\n";
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\ncontent-type: text/markdown\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(body);
    });
    let target = url(address, "/late.md");
    let ctx = crate::test_context(
        Box::new(crate::parser::TreeSitterProvider::new()),
        crate::config::Config {
            url_fetch_allow_private: true,
            storage_dir: Some(storage.path().to_path_buf()),
            ..Default::default()
        },
    );
    let req: crate::protocol::RawRequest = serde_json::from_value(serde_json::json!({
        "id": "late-url",
        "command": "outline",
        "file": target,
    }))
    .unwrap();
    // The first filesystem access to the downloaded document never finishes.
    let document = cache_content_path_for_url(storage.path(), &target, ".md");
    let stalled_read_back = crate::bounded_io::stall_running_helper(&document);
    let _call = crate::bounded_io::enter(Some(crate::bounded_io::Budget::after(budget)));
    let started = Instant::now();
    let response = crate::commands::outline::handle_outline(&req, &ctx);
    let elapsed = started.elapsed();
    drop(stalled_read_back);

    assert!(
        elapsed >= answer_after,
        "the download completed: {elapsed:?}"
    );
    assert!(document.exists(), "the download reached the cache");
    assert!(!response.success, "{:?}", response.data);
    assert_ne!(
        response.data["code"], "url_fetch_timeout",
        "{:?}",
        response.data
    );
    // The stalled read gives up before the reply margin, so the reply is
    // ready with that margin to spare.
    assert!(
        elapsed + URL_REPLY_MARGIN / 2 < budget,
        "the reply came {elapsed:?} into a {budget:?} call"
    );
}
