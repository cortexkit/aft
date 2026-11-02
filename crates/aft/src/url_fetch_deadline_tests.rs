//! URL fetches against local test servers: deadlines for servers that stall
//! in one phase, remembered failures, and reuse of the document cache. Most
//! tests run the fetch under a short call budget, the same way a tool call
//! does, and check the typed timeout and how long it took.

use super::*;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Starts a loopback server. `handler` gets each connection after the request
/// head has been read, together with the request path.
fn serve(handler: impl Fn(&str, TcpStream) + Send + Sync + 'static) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
    let address = listener.local_addr().expect("test server address");
    let handler = Arc::new(handler);
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let handler = Arc::clone(&handler);
            thread::spawn(move || {
                let mut stream = stream;
                let path = read_request_path(&mut stream);
                handler(&path, stream);
            });
        }
    });
    address
}

fn read_request_path(stream: &mut TcpStream) -> String {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut head = Vec::new();
    let mut buffer = [0u8; 1024];
    while !head.windows(4).any(|window| window == b"\r\n\r\n") {
        match stream.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(n) => head.extend_from_slice(&buffer[..n]),
        }
    }
    String::from_utf8_lossy(&head)
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .to_string()
}

fn url(address: SocketAddr, path: &str) -> String {
    format!("http://127.0.0.1:{}{path}", address.port())
}

fn local() -> UrlFetchOptions {
    UrlFetchOptions {
        allow_private: true,
        ..UrlFetchOptions::default()
    }
}

/// Runs one fetch inside a call budget of `budget`, as a tool call would.
fn fetch_within(
    budget: Duration,
    url: &str,
    storage: &Path,
    options: UrlFetchOptions,
) -> (Result<PathBuf, UrlFetchError>, Duration) {
    let _call = crate::bounded_io::enter(Some(crate::bounded_io::Budget::after(budget)));
    let started = Instant::now();
    let result = fetch_url_to_cache(url, storage, options);
    (result, started.elapsed())
}

/// The call budget most tests run under. The fetch deadline is 2 s of it:
/// the rest is the read-back reserve and the reply margin.
const CALL_BUDGET: Duration = Duration::from_secs(5);
/// A fetch under [`CALL_BUDGET`] must have returned by this time: its 2 s
/// deadline plus 1 s for scheduling on a loaded machine.
const FETCH_DONE_BY: Duration = Duration::from_secs(3);

/// Long enough that a fetch which ignored its deadline could not finish a
/// test in time, short enough that leftover server threads end soon.
const SERVER_STALL: Duration = Duration::from_secs(60);

#[test]
fn fetch_deadline_is_the_call_budget_less_the_read_back_reserve_and_reply_margin_capped_at_the_ceiling(
) {
    let near = |deadline: FetchDeadline, expected: Duration| {
        let allowed = deadline.allowed;
        assert!(
            allowed <= expected && allowed + Duration::from_millis(200) >= expected,
            "allowed {allowed:?}, expected about {expected:?}"
        );
    };
    near(FetchDeadline::for_current_call(), URL_FETCH_CEILING);
    for (budget, expected) in [
        (Duration::from_secs(5), Duration::from_secs(2)),
        (Duration::from_secs(60), URL_FETCH_CEILING),
        (Duration::from_millis(2_500), Duration::ZERO),
    ] {
        let _call = crate::bounded_io::enter(Some(crate::bounded_io::Budget::after(budget)));
        near(FetchDeadline::for_current_call(), expected);
    }
}

#[test]
fn header_stall_times_out_in_the_headers_phase_within_the_call_budget() {
    let storage = tempfile::tempdir().unwrap();
    let address = serve(|_, _stream| thread::sleep(SERVER_STALL));
    let (result, elapsed) = fetch_within(
        CALL_BUDGET,
        &url(address, "/silent.md"),
        storage.path(),
        local(),
    );
    let error = result.expect_err("a server that never answers must fail");
    assert_eq!(error.code(), "url_fetch_timeout", "{error}");
    assert_eq!(error.phase(), Some(FetchPhase::Headers), "{error}");
    assert!(error.to_string().contains("headers phase"), "{error}");
    assert!(
        elapsed < FETCH_DONE_BY,
        "the fetch outlived its deadline: {elapsed:?}"
    );
}

#[test]
fn body_drip_stops_at_the_whole_body_deadline() {
    let storage = tempfile::tempdir().unwrap();
    // Each gap is far below the per-chunk stall limit, so only a deadline for
    // the whole body can stop this response.
    let address = serve(|_, mut stream| {
        let _ = stream.write_all(
            b"HTTP/1.1 200 OK\r\ncontent-type: text/markdown\r\nconnection: close\r\n\r\n",
        );
        let started = Instant::now();
        while started.elapsed() < SERVER_STALL {
            if stream.write_all(b"## drip\n\nline\n\n").is_err() {
                return;
            }
            thread::sleep(Duration::from_millis(200));
        }
    });
    let (result, elapsed) = fetch_within(
        CALL_BUDGET,
        &url(address, "/drip.md"),
        storage.path(),
        local(),
    );
    let error = result.expect_err("an endless body must fail at the deadline");
    assert_eq!(error.code(), "url_fetch_timeout", "{error}");
    assert_eq!(error.phase(), Some(FetchPhase::Body), "{error}");
    assert!(
        elapsed < FETCH_DONE_BY,
        "the body read outlived its deadline: {elapsed:?}"
    );
}

/// A server that accepts and closes every connection without answering, which
/// reqwest reports as a transient failure worth a retry.
fn hang_up_server() -> (SocketAddr, Arc<AtomicUsize>) {
    let accepted = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&accepted);
    let address = serve(move |_, stream| {
        counter.fetch_add(1, Ordering::SeqCst);
        drop(stream);
    });
    (address, accepted)
}

#[test]
fn transient_failures_retry_while_the_backoff_and_an_attempt_fit() {
    let storage = tempfile::tempdir().unwrap();
    let (address, accepted) = hang_up_server();
    let (result, _) = fetch_within(
        Duration::from_secs(10),
        &url(address, "/closed.md"),
        storage.path(),
        local(),
    );
    assert!(result.is_err());
    assert_eq!(
        accepted.load(Ordering::SeqCst),
        1 + TRANSIENT_RETRY_ATTEMPTS
    );
}

#[test]
fn transient_failures_do_not_retry_when_the_backoff_and_an_attempt_do_not_fit() {
    let storage = tempfile::tempdir().unwrap();
    let (address, accepted) = hang_up_server();
    // 3.6 s of budget leaves a 0.6 s fetch deadline: less than the first
    // backoff plus one useful attempt.
    let (result, elapsed) = fetch_within(
        Duration::from_millis(3_600),
        &url(address, "/closed.md"),
        storage.path(),
        local(),
    );
    assert!(result.is_err());
    assert_eq!(accepted.load(Ordering::SeqCst), 1);
    assert!(elapsed < Duration::from_millis(600), "{elapsed:?}");
}

#[test]
fn outline_reports_the_typed_timeout_and_its_phase() {
    let storage = tempfile::tempdir().unwrap();
    let address = serve(|_, _stream| thread::sleep(SERVER_STALL));
    let ctx = crate::context::AppContext::new(
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

fn stalled_lookup() -> HostLookup {
    Arc::new(|_host: &str| {
        thread::sleep(SERVER_STALL);
        Ok(vec![IpAddr::V4(Ipv4Addr::LOCALHOST)])
    })
}

#[test]
fn stalled_name_lookup_times_out_in_the_dns_phase_before_the_ssrf_check_answers() {
    let storage = tempfile::tempdir().unwrap();
    let options = UrlFetchOptions {
        lookup_override: Some(stalled_lookup()),
        ..UrlFetchOptions::default()
    };
    let (result, elapsed) = fetch_within(
        CALL_BUDGET,
        "http://slow-dns.test/doc.md",
        storage.path(),
        options,
    );
    let error = result.expect_err("a lookup that never answers must fail");
    assert_eq!(error.code(), "url_fetch_timeout", "{error}");
    assert_eq!(error.phase(), Some(FetchPhase::Dns), "{error}");
    assert!(elapsed < FETCH_DONE_BY, "{elapsed:?}");
}

#[test]
fn stalled_name_lookup_inside_the_connection_times_out_in_the_dns_phase() {
    // With private addresses allowed there is no SSRF lookup first, so the
    // connection's own lookup is the one that stalls.
    let storage = tempfile::tempdir().unwrap();
    let options = UrlFetchOptions {
        allow_private: true,
        lookup_override: Some(stalled_lookup()),
        ..UrlFetchOptions::default()
    };
    let (result, elapsed) = fetch_within(
        CALL_BUDGET,
        "http://slow-dns.test/doc.md",
        storage.path(),
        options,
    );
    let error = result.expect_err("a lookup that never answers must fail");
    assert_eq!(error.code(), "url_fetch_timeout", "{error}");
    assert_eq!(error.phase(), Some(FetchPhase::Dns), "{error}");
    assert!(elapsed < FETCH_DONE_BY, "{elapsed:?}");
}

#[test]
fn host_resolving_to_a_private_address_is_refused() {
    let storage = tempfile::tempdir().unwrap();
    let options = UrlFetchOptions {
        lookup_override: Some(Arc::new(|_host: &str| {
            Ok(vec![IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5))])
        })),
        ..UrlFetchOptions::default()
    };
    let (result, _) = fetch_within(
        CALL_BUDGET,
        "http://intranet.test/doc.md",
        storage.path(),
        options,
    );
    let error = result.expect_err("a private answer must be refused");
    assert!(
        error.to_string().contains("Blocked private URL host"),
        "{error}"
    );
}

#[test]
fn connection_uses_the_addresses_the_ssrf_check_approved_without_a_second_lookup() {
    // The first answer is public; any later answer is loopback, as a
    // DNS-rebinding server would reply. The connection must get the first.
    let lookups = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&lookups);
    let approved_ip = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 9));
    let options = UrlFetchOptions {
        lookup_override: Some(Arc::new(move |_host: &str| {
            let first = counter.fetch_add(1, Ordering::SeqCst) == 0;
            Ok(vec![if first {
                approved_ip
            } else {
                IpAddr::V4(Ipv4Addr::LOCALHOST)
            }])
        })),
        ..UrlFetchOptions::default()
    };
    let deadline = FetchDeadline::for_current_call();
    let approved = Arc::new(ApprovedHosts::default());
    let target = Url::parse("http://rebind.test/doc.md").unwrap();
    validate_public_url(&target, &options, &deadline, &approved).expect("public answer");

    let resolver = GuardedResolver {
        approved,
        connect_overrides: Vec::new(),
        lookup_override: options.lookup_override.clone(),
        progress: Arc::new(ConnectProgress::default()),
    };
    let name: reqwest::dns::Name = "rebind.test".parse().unwrap();
    let addresses: Vec<SocketAddr> = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(reqwest::dns::Resolve::resolve(&resolver, name))
        .expect("approved host resolves")
        .collect();
    assert_eq!(addresses, vec![SocketAddr::new(approved_ip, 0)]);
    assert_eq!(lookups.load(Ordering::SeqCst), 1, "no second lookup");
}

/// Counts requests per path and answers each path with the status its name
/// gives (`/503`, `/404`, ...); `/flaky` answers 503 once and 200 after.
fn status_server() -> (SocketAddr, Arc<std::sync::Mutex<HashMap<String, usize>>>) {
    let seen = Arc::new(std::sync::Mutex::new(HashMap::<String, usize>::new()));
    let counter = Arc::clone(&seen);
    let address = serve(move |path, mut stream| {
        let count = {
            let mut seen = counter.lock().unwrap();
            let count = seen.entry(path.to_string()).or_default();
            *count += 1;
            *count
        };
        let status = match path.trim_start_matches('/') {
            "flaky" if count > 1 => "200 OK",
            "flaky" | "503" => "503 Service Unavailable",
            "404" => "404 Not Found",
            "408" => "408 Request Timeout",
            "429" => "429 Too Many Requests",
            _ => "200 OK",
        };
        let body = b"# Doc\n\n## Section\n\ntext\n";
        let _ = write!(
            stream,
            "HTTP/1.1 {status}\r\ncontent-type: text/markdown\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(body);
    });
    (address, seen)
}

#[test]
fn a_server_that_hit_the_headers_limit_fails_from_memory_without_a_new_fetch() {
    let storage = tempfile::tempdir().unwrap();
    let connections = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&connections);
    let address = serve(move |_, _stream| {
        counter.fetch_add(1, Ordering::SeqCst);
        thread::sleep(SERVER_STALL);
    });
    let target = url(address, "/silent.md");
    // A budget large enough that the 15 s headers limit, not the caller's
    // budget, ends the wait: the server is to blame.
    let roomy = Duration::from_secs(30);
    let (first, _) = fetch_within(roomy, &target, storage.path(), local());
    assert_eq!(first.unwrap_err().code(), "url_fetch_timeout");

    let (second, elapsed) = fetch_within(roomy, &target, storage.path(), local());
    let error = second.expect_err("the remembered failure is repeated");
    // A new fetch would wait out the 15 s headers limit again, so anything well
    // under that proves the answer came from memory. The connection count
    // below is the exact check; this bound only has to survive a loaded runner
    // (a Windows CI run took 212 ms).
    assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
    assert_eq!(error.code(), "url_fetch_timeout", "{error}");
    assert_eq!(error.phase(), Some(FetchPhase::Headers), "{error}");
    assert!(error.to_string().contains("cached failure"), "{error}");
    assert!(error.to_string().contains("fetched again after"), "{error}");
    let retry_after = error.retry_after().expect("a remembered failure says when");
    assert!(retry_after <= FAILURE_MEMORY && !retry_after.is_zero());
    assert_eq!(connections.load(Ordering::SeqCst), 1, "no second fetch");
}

/// Answers every request with a small Markdown document, after stalling
/// the first one; counts requests.
fn answers_after_a_first_stall(stall_first: bool) -> (SocketAddr, Arc<AtomicUsize>) {
    let requests = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&requests);
    let address = serve(move |_, mut stream| {
        if counter.fetch_add(1, Ordering::SeqCst) == 0 && stall_first {
            thread::sleep(SERVER_STALL);
            return;
        }
        let body = b"# Doc\n\n## Section\n\ntext\n";
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\ncontent-type: text/markdown\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(body);
    });
    (address, requests)
}

#[test]
fn a_fetch_never_started_for_lack_of_budget_is_not_remembered() {
    let storage = tempfile::tempdir().unwrap();
    let (address, requests) = answers_after_a_first_stall(false);
    let target = url(address, "/late-in-a-batch.md");
    // Less budget than the read-back reserve and reply margin: no time is
    // left to fetch at all, as for a late URL in a long zoom batch.
    let (starved, _) = fetch_within(
        Duration::from_millis(2_900),
        &target,
        storage.path(),
        local(),
    );
    assert_eq!(starved.unwrap_err().code(), "url_fetch_timeout");
    assert_eq!(
        requests.load(Ordering::SeqCst),
        0,
        "the fetch never started"
    );

    let (fresh, _) = fetch_within(CALL_BUDGET, &target, storage.path(), local());
    fresh.expect("a call with a fresh budget fetches normally");
    assert_eq!(requests.load(Ordering::SeqCst), 1);
}

#[test]
fn a_fetch_cut_short_by_the_call_budget_is_not_remembered() {
    let storage = tempfile::tempdir().unwrap();
    let (address, requests) = answers_after_a_first_stall(true);
    let target = url(address, "/slow-once.md");
    // The 2 s fetch deadline this budget leaves ends the first request
    // before any limit of the fetcher's own would.
    let (cut, _) = fetch_within(CALL_BUDGET, &target, storage.path(), local());
    let error = cut.expect_err("the stalled first request is cut off");
    assert_eq!(error.phase(), Some(FetchPhase::Headers), "{error}");

    let (fresh, _) = fetch_within(CALL_BUDGET, &target, storage.path(), local());
    fresh.expect("a call with a fresh budget fetches normally");
    assert_eq!(requests.load(Ordering::SeqCst), 2);
}

#[test]
fn a_cached_document_is_served_while_a_failure_is_remembered() {
    let storage = tempfile::tempdir().unwrap();
    let (address, requests) = answers_after_a_first_stall(false);
    let target = url(address, "/cached.md");
    let (first, _) = fetch_within(CALL_BUDGET, &target, storage.path(), local());
    let cached = first.expect("first fetch");
    remember_failure(
        FailureKey::new(storage.path(), &target, true),
        &UrlFetchError::http_status(503, "HTTP 503 Service Unavailable"),
    );

    let (second, _) = fetch_within(CALL_BUDGET, &target, storage.path(), local());
    assert_eq!(second.expect("the cached document is served"), cached);
    assert_eq!(requests.load(Ordering::SeqCst), 1, "no new fetch");
}

#[test]
fn server_errors_and_client_errors_other_than_408_and_429_are_remembered() {
    let storage = tempfile::tempdir().unwrap();
    let (address, seen) = status_server();
    for path in ["/503", "/404", "/408", "/429"] {
        for _ in 0..2 {
            let (result, _) =
                fetch_within(CALL_BUDGET, &url(address, path), storage.path(), local());
            assert!(result.is_err(), "{path}");
        }
    }
    let seen = seen.lock().unwrap();
    assert_eq!(seen.get("/503"), Some(&1), "{seen:?}");
    assert_eq!(seen.get("/404"), Some(&1), "{seen:?}");
    assert_eq!(seen.get("/408"), Some(&2), "{seen:?}");
    assert_eq!(seen.get("/429"), Some(&2), "{seen:?}");
}

#[test]
fn a_remembered_failure_expires_and_the_url_is_fetched_again() {
    let storage = tempfile::tempdir().unwrap();
    let (address, seen) = status_server();
    let target = url(address, "/flaky");
    let (first, _) = fetch_within(CALL_BUDGET, &target, storage.path(), local());
    assert!(first.unwrap_err().to_string().contains("HTTP 503"));

    age_remembered_failure(
        &FailureKey::new(storage.path(), &target, true),
        FAILURE_MEMORY + Duration::from_secs(1),
    );
    let (second, _) = fetch_within(CALL_BUDGET, &target, storage.path(), local());
    second.expect("an expired failure no longer stands in for the fetch");
    assert_eq!(seen.lock().unwrap().get("/flaky"), Some(&2));
}

#[test]
fn outline_says_a_repeated_failure_is_cached_and_when_it_is_fetched_again() {
    let storage = tempfile::tempdir().unwrap();
    let (address, _) = status_server();
    let ctx = crate::context::AppContext::new(
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

#[test]
fn a_silent_server_gets_the_headers_limit_and_no_retry() {
    let storage = tempfile::tempdir().unwrap();
    let connections = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&connections);
    let address = serve(move |_, _stream| {
        counter.fetch_add(1, Ordering::SeqCst);
        thread::sleep(SERVER_STALL);
    });
    // The budget leaves the full 20 s ceiling, so the 15 s headers limit is
    // what ends the wait.
    let (result, elapsed) = fetch_within(
        Duration::from_secs(30),
        &url(address, "/silent.md"),
        storage.path(),
        local(),
    );
    let error = result.expect_err("a silent server must fail");
    assert_eq!(error.phase(), Some(FetchPhase::Headers), "{error}");
    assert!(error.to_string().contains("15000ms limit"), "{error}");
    assert!(
        elapsed >= HEADERS_TIMEOUT && elapsed < URL_FETCH_CEILING,
        "{elapsed:?}"
    );
    assert_eq!(connections.load(Ordering::SeqCst), 1);
}

#[test]
fn a_stalled_connect_ends_at_the_connect_limit() {
    let storage = tempfile::tempdir().unwrap();
    let address = serve(|_, _stream| thread::sleep(SERVER_STALL));
    let options = UrlFetchOptions {
        connect_delay: Some(SERVER_STALL),
        ..local()
    };
    // A 14 s budget leaves an 11 s fetch deadline: the 10 s connect limit ends the first attempt,
    // and the second does not fit in what is left.
    let (result, elapsed) = fetch_within(
        Duration::from_secs(14),
        &url(address, "/unreachable.md"),
        storage.path(),
        options,
    );
    let error = result.expect_err("a connect that never finishes must fail");
    assert_eq!(error.phase(), Some(FetchPhase::Connect), "{error}");
    assert!(error.to_string().contains("10000ms limit"), "{error}");
    assert!(
        elapsed >= CONNECT_TIMEOUT && elapsed < Duration::from_secs(11),
        "{elapsed:?}"
    );
}

#[test]
fn a_retry_stopped_by_the_deadline_while_connecting_is_named_the_connect_phase() {
    let storage = tempfile::tempdir().unwrap();
    let address = serve(|_, _stream| thread::sleep(SERVER_STALL));
    let options = UrlFetchOptions {
        connect_delay: Some(SERVER_STALL),
        ..local()
    };
    let (result, elapsed) = fetch_within(
        Duration::from_secs(30),
        &url(address, "/unreachable.md"),
        storage.path(),
        options,
    );
    let error = result.expect_err("a connect that never finishes must fail");
    // The first attempt ends at the 10 s connect limit and is retried; the
    // retry has less than 10 s left, so the fetch deadline ends it while it
    // is still connecting, and it must still be named the connect phase.
    assert_eq!(error.code(), "url_fetch_timeout", "{error}");
    assert_eq!(error.phase(), Some(FetchPhase::Connect), "{error}");
    assert!(error.to_string().contains("fetch deadline"), "{error}");
    assert!(
        elapsed >= URL_FETCH_CEILING - Duration::from_millis(500)
            && elapsed < URL_FETCH_CEILING + Duration::from_secs(2),
        "{elapsed:?}"
    );
}

#[test]
fn a_cached_html_page_is_served_from_the_cache_without_a_new_fetch() {
    let storage = tempfile::tempdir().unwrap();
    let requests = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&requests);
    let address = serve(move |_, mut stream| {
        counter.fetch_add(1, Ordering::SeqCst);
        let body = b"<html><body><h2>Heading 1</h2><p>text</p></body></html>";
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(body);
    });
    let target = url(address, "/guide/page.html");
    let (first, _) = fetch_within(Duration::from_secs(5), &target, storage.path(), local());
    let first = first.expect("first fetch");
    let modified = fs::metadata(&first).unwrap().modified().unwrap();
    let (second, _) = fetch_within(Duration::from_secs(5), &target, storage.path(), local());
    let second = second.expect("cached copy");
    assert_eq!(first, second);
    assert_eq!(
        requests.load(Ordering::SeqCst),
        1,
        "the page was fetched again"
    );
    // An unchanged file keeps the parsed symbols of the last call valid.
    assert_eq!(fs::metadata(&second).unwrap().modified().unwrap(), modified);
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
    let ctx = crate::context::AppContext::new(
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

#[test]
fn a_stalled_cache_check_returns_the_remembered_failure_within_the_budget_and_is_not_repeated() {
    let storage = tempfile::tempdir().unwrap();
    // No server: with the failure remembered, nothing may be fetched.
    let target = "http://127.0.0.1:9/never-fetched.md";
    remember_failure(
        FailureKey::new(storage.path(), target, true),
        &UrlFetchError::http_status(503, "HTTP 503 Service Unavailable"),
    );
    let stall = stall_cache_reads(storage.path());
    let budget = Duration::from_secs(2);
    for call in 1..=2 {
        let (result, elapsed) = fetch_within(budget, target, storage.path(), local());
        let error = result.expect_err("the remembered failure answers");
        assert!(error.retry_after().is_some(), "call {call}: {error}");
        assert!(
            error.to_string().contains("HTTP 503"),
            "call {call}: {error}"
        );
        // The check waits until one reply margin before the end of the call.
        assert!(
            elapsed + URL_REPLY_MARGIN / 2 < budget,
            "call {call} answered {elapsed:?} into a {budget:?} call"
        );
    }
    assert_eq!(
        stall.reached.load(Ordering::SeqCst),
        1,
        "the second call joined the stalled check instead of starting another"
    );
    stall.release();
}

/// A read failure the way reqwest's blocking body reader reports its own
/// timeouts: an `io::Error` of kind `Other`, not `TimedOut`.
const REQWEST_STYLE_TIMEOUT: io::ErrorKind = io::ErrorKind::Other;

#[test]
fn a_generic_read_error_after_the_fetch_deadline_is_a_body_timeout() {
    // The deadline has already passed when the failed read is reported, as
    // when reqwest's whole-request timer fires before this side's own wait.
    let deadline = FetchDeadline {
        at: Instant::now(),
        allowed: Duration::from_millis(2_000),
        set_by_call: true,
    };
    let error = body_read_failure(
        &deadline,
        "http://example.test/doc.md",
        REQWEST_STYLE_TIMEOUT,
        "error decoding response body: operation timed out",
        Duration::from_millis(1_990),
    );
    assert_eq!(error.code(), "url_fetch_timeout", "{error}");
    assert_eq!(error.phase(), Some(FetchPhase::Body), "{error}");
}

#[test]
fn a_generic_read_error_after_the_chunk_stall_limit_is_a_body_timeout() {
    let deadline = FetchDeadline {
        at: Instant::now() + Duration::from_secs(5),
        allowed: URL_FETCH_CEILING,
        set_by_call: false,
    };
    let error = body_read_failure(
        &deadline,
        "http://example.test/doc.md",
        REQWEST_STYLE_TIMEOUT,
        "error decoding response body: operation timed out",
        BODY_CHUNK_TIMEOUT,
    );
    assert_eq!(error.code(), "url_fetch_timeout", "{error}");
    assert_eq!(error.phase(), Some(FetchPhase::Body), "{error}");
    assert!(error.to_string().contains("Body read stalled"), "{error}");
}

#[test]
fn a_generic_read_error_before_any_limit_is_a_read_failure() {
    let deadline = FetchDeadline {
        at: Instant::now() + Duration::from_secs(5),
        allowed: URL_FETCH_CEILING,
        set_by_call: false,
    };
    let error = body_read_failure(
        &deadline,
        "http://example.test/doc.md",
        REQWEST_STYLE_TIMEOUT,
        "connection reset by peer",
        Duration::from_millis(10),
    );
    assert_eq!(error.code(), "url_fetch_failed", "{error}");
    assert_eq!(error.phase(), None, "{error}");
}
