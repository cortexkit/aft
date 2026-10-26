use aft::callgraph_store::CallGraphStore;
use aft::commands::{semantic_search::handle_semantic_search, tool_call};
use aft::config::{Config, GatherConfig, GatherDecisionBackend, GatherDecisionConfig};
use aft::context::AppContext;
use aft::parser::TreeSitterProvider;
use aft::protocol::{RawRequest, Response};
use aft::search_index::SearchIndex;
use serde_json::json;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const SOURCE: &str = "/// Load application config settings.\npub fn load_config() { parse_config(); }\nfn parse_config() { read_config(); }\nfn read_config() {}\n";

struct Fixture {
    _temp: tempfile::TempDir,
    ctx: AppContext,
}

impl Fixture {
    fn new(ready: bool) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let file = root.join("main.rs");
        std::fs::write(&file, SOURCE).unwrap();
        let mut config = crate::context_storage::isolate(Config {
            project_root: Some(root.clone()),
            ..Config::default()
        });
        config.views.enabled = false;
        config.indexes.callgraph = ready;
        let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), config)
            .with_test_checkpoint_store();
        let mut index = SearchIndex::new();
        index.index_file(&file, SOURCE.as_bytes());
        index.ready = true;
        *ctx.search_index().write().unwrap() = Some(index);
        if ready {
            let key = aft::search_index::artifact_cache_key(&root);
            aft::root_cache::configure_artifact_access(&root, &key, false);
            let dir = temp.path().join("callgraph").join(key);
            let (_writer, _) =
                CallGraphStore::cold_build_with_lease(dir.clone(), root.clone(), &[file]).unwrap();
            let store = CallGraphStore::open_readonly(dir, root).unwrap().unwrap();
            let nodes = _writer
                .nodes_for(std::path::Path::new("main.rs"), "load_config")
                .unwrap();
            assert!(
                !nodes.is_empty(),
                "fixture must contain the seed selection range"
            );
            *ctx.callgraph_store().write().unwrap() = Some(Arc::new(store));
        }
        Self { _temp: temp, ctx }
    }

    fn search(
        &self,
        enabled: bool,
        trace: Option<&str>,
        query: &str,
        pattern: Option<&str>,
        wrapped: bool,
    ) -> Response {
        self.search_session(enabled, trace, query, pattern, wrapped, "session-a")
    }

    fn search_session(
        &self,
        enabled: bool,
        trace: Option<&str>,
        query: &str,
        pattern: Option<&str>,
        wrapped: bool,
        session: &str,
    ) -> Response {
        self.ctx.update_config(|config| {
            config
                .search
                .gather
                .get_or_insert_with(GatherConfig::default)
                .enabled = Some(enabled)
        });
        let mut args = json!({"query": query, "top_k": 5});
        if let Some(pattern) = pattern {
            args["pattern"] = json!(pattern);
        }
        if wrapped {
            let mut value = json!({"id":"gather-search", "session_id":session, "command":"tool_call", "name":"search", "arguments":args});
            if let Some(trace) = trace {
                value["search_context"] = json!(trace);
            }
            let req: RawRequest = serde_json::from_value(value).unwrap();
            tool_call::handle_with_dispatch(&req, &self.ctx, &|req, ctx| {
                handle_semantic_search(&req, ctx)
            })
        } else {
            args["id"] = json!("gather-search");
            args["command"] = json!("semantic_search");
            args["session_id"] = json!(session);
            if let Some(trace) = trace {
                args["search_context"] = json!(trace);
            }
            handle_semantic_search(&serde_json::from_value(args).unwrap(), &self.ctx)
        }
    }
}

const QUERY: &str = "how is application config loaded";

struct Endpoint {
    url: String,
    requests: Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
    stopped: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Endpoint {
    fn new(status: u16, delay: Duration, stop: bool) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = requests.clone();
        let stopped = Arc::new(AtomicBool::new(false));
        let done = stopped.clone();
        let thread = std::thread::spawn(move || {
            while let Ok((mut stream, _)) = listener.accept() {
                if done.load(Ordering::Acquire) {
                    break;
                }
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut bytes = Vec::new();
                let (start, length) = loop {
                    let mut buffer = [0; 4096];
                    let read = stream.read(&mut buffer).unwrap();
                    assert!(read > 0, "endpoint request must complete");
                    bytes.extend_from_slice(&buffer[..read]);
                    if let Some(start) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                        let headers = std::str::from_utf8(&bytes[..start]).unwrap();
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length: ")
                                    .map(|n| n.parse::<usize>().unwrap())
                            })
                            .unwrap();
                        break (start + 4, length);
                    }
                };
                while bytes.len() < start + length {
                    let mut buffer = [0; 4096];
                    let read = stream.read(&mut buffer).unwrap();
                    assert!(read > 0);
                    bytes.extend_from_slice(&buffer[..read]);
                }
                let request: serde_json::Value =
                    serde_json::from_slice(&bytes[start..start + length]).unwrap();
                let native = request.get("state").is_some();
                let payload: serde_json::Value = if native {
                    request["state"].clone()
                } else {
                    serde_json::from_str(request["messages"][1]["content"].as_str().unwrap())
                        .unwrap()
                };
                captured.lock().unwrap().push(payload.clone());
                if !delay.is_zero() {
                    let (_hold, wait) = std::sync::mpsc::channel::<()>();
                    let _ = wait.recv_timeout(delay);
                }
                let ids: Vec<_> = payload[if native { "candidates" } else { "frontier" }]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|node| node["id"].clone())
                    .collect();
                let decision =
                    json!({"keep":ids,"expand":if stop {vec![]} else {ids.clone()},"stop":stop});
                let body = if native {
                    let mut answers = serde_json::Map::new();
                    for i in 0..ids.len() {
                        answers.insert(format!("keep_{i}"), json!({"type":"noul","noul":1.0}));
                        answers.insert(format!("expand_{i}"), json!({"type":"noul","noul":0.0}));
                    }
                    answers.insert("stop".into(), json!({"type":"noul","noul":1.0}));
                    json!({"answers":answers}).to_string()
                } else {
                    json!({"choices":[{"message":{"content":decision.to_string()}}]}).to_string()
                };
                let _ = write!(stream, "HTTP/1.1 {status} Reply\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len());
            }
        });
        Self {
            url: format!("http://{address}/v1"),
            requests,
            stopped,
            thread: Some(thread),
        }
    }

    fn count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }

    fn configure(&self, f: &Fixture, decision_ms: u64, walk_ms: u64) {
        f.ctx.update_config(|config| {
            config.search.gather = Some(GatherConfig {
                enabled: Some(true),
                decision: Some(GatherDecisionConfig {
                    backend: Some(GatherDecisionBackend::Endpoint),
                    endpoint: Some(self.url.clone()),
                    model: Some("boundary-fixture".into()),
                    ..Default::default()
                }),
                decision_timeout_ms: Some(decision_ms),
                deadline_ms: Some(walk_ms),
                ..Default::default()
            });
        });
    }
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        let url = url::Url::parse(&self.url).unwrap();
        let _ = std::net::TcpStream::connect(("127.0.0.1", url.port().unwrap()));
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

#[test]
fn gather_search_test_native_decisions_full_walk() {
    let f = Fixture::new(true);
    let endpoint = Endpoint::new(200, Duration::ZERO, true);
    endpoint.configure(&f, 2000, 6000);
    f.ctx.update_config(|config| {
        config.search.gather.as_mut().unwrap().decision = Some(
            serde_json::from_value(json!({
                "backend":"decisions", "endpoint":endpoint.url, "model":"mercury-decide"
            }))
            .unwrap(),
        );
    });
    let response = f.search(true, None, QUERY, None, true);
    assert!(response.success);
    let text = response.data["text"].as_str().unwrap();
    assert!(text.contains("model=mercury-decide"), "{text}");
    assert!(text.contains("parse_config"), "{text}");
    assert!(!text.contains("fallback=1:"), "{text}");
    assert_eq!(endpoint.count(), 1);
}

#[test]
fn query_only_endpoint_works() {
    for wrapped in [false, true] {
        for trace in [None, Some("")] {
            let f = Fixture::new(true);
            let endpoint = Endpoint::new(200, Duration::ZERO, true);
            endpoint.configure(&f, 2000, 6000);
            let response = f.search(true, trace, QUERY, None, wrapped);
            assert!(response.success);
            let text = response.data["text"].as_str().unwrap();
            assert!(text.contains("model=boundary-fixture"), "{text}");
            assert!(text.contains("context: query only"), "{text}");
            assert_eq!(endpoint.count(), 1);
            assert!(endpoint.requests.lock().unwrap()[0]["trace"].is_null() || trace == Some(""));
        }
    }
}

#[test]
fn same_session_repeat_calls_endpoint_once() {
    for wrapped in [false, true] {
        let f = Fixture::new(true);
        let endpoint = Endpoint::new(200, Duration::ZERO, true);
        endpoint.configure(&f, 2000, 6000);
        for _ in 0..2 {
            assert!(
                f.search(true, Some("trace-a"), QUERY, None, wrapped)
                    .success
            );
        }
        assert_eq!(endpoint.count(), 1, "repeat must use the session cache");
        assert!(
            f.search(true, Some("trace-b"), QUERY, None, wrapped)
                .success
        );
        assert_eq!(endpoint.count(), 2, "trace change must miss cache");
        f.ctx.update_config(|config| {
            config
                .search
                .gather
                .as_mut()
                .unwrap()
                .decision
                .as_mut()
                .unwrap()
                .model = Some("changed-model".into())
        });
        assert!(
            f.search(true, Some("trace-b"), QUERY, None, wrapped)
                .success
        );
        assert_eq!(
            endpoint.count(),
            3,
            "endpoint configuration change must miss cache"
        );
        let root = f.ctx.config().project_root.clone().unwrap();
        let key = aft::search_index::artifact_cache_key(&root);
        let dir = f._temp.path().join("replacement").join(key);
        let (_writer, _) = CallGraphStore::cold_build_with_lease(
            dir.clone(),
            root.clone(),
            &[root.join("main.rs")],
        )
        .unwrap();
        *f.ctx.callgraph_store().write().unwrap() = Some(Arc::new(
            CallGraphStore::open_readonly(dir, root).unwrap().unwrap(),
        ));
        assert!(
            f.search(true, Some("trace-b"), QUERY, None, wrapped)
                .success
        );
        assert_eq!(
            endpoint.count(),
            4,
            "graph generation change must miss cache"
        );
    }
}

#[test]
fn different_sessions_do_not_share_decisions() {
    for wrapped in [false, true] {
        let f = Fixture::new(true);
        let endpoint = Endpoint::new(200, Duration::ZERO, true);
        endpoint.configure(&f, 2000, 6000);
        for session in ["a", "a", "b", "b"] {
            assert!(
                f.search_session(true, Some("same-trace"), QUERY, None, wrapped, session)
                    .success
            );
        }
        assert_eq!(
            endpoint.count(),
            2,
            "each session must make its own decision request"
        );
    }
}

#[test]
fn endpoint_error_uses_deterministic_evidence() {
    for wrapped in [false, true] {
        let f = Fixture::new(true);
        let endpoint = Endpoint::new(503, Duration::ZERO, false);
        endpoint.configure(&f, 2000, 6000);
        let plain = f.search(false, None, QUERY, None, wrapped);
        let response = f.search(true, None, QUERY, None, wrapped);
        assert_ranked_unchanged(&plain, &response);
        let text = response.data["text"].as_str().unwrap();
        assert!(
            text.contains("parse_config: function"),
            "failed decision must retain deterministic neighbour evidence: {text}"
        );
        assert!(text.contains("1:unavailable"), "{text}");
        assert!(endpoint.count() >= 2);
    }
}

#[test]
fn full_deadline_bounds_rounds_and_fallback() {
    for wrapped in [false, true] {
        let f = Fixture::new(true);
        let endpoint = Endpoint::new(503, Duration::from_millis(900), false);
        endpoint.configure(&f, 2000, 100);
        let start = Instant::now();
        let response = f.search(true, None, QUERY, None, wrapped);
        let elapsed = start.elapsed();
        assert!(response.success);
        let text = response.data["text"].as_str().unwrap();
        assert!(
            elapsed < Duration::from_millis(700),
            "full deadline did not bound request: {elapsed:?}"
        );
        assert!(text.contains("stop=deadline"), "{text}");
        assert!(
            text.contains("kept=1"),
            "timeout must preserve fallback evidence: {text}"
        );
        assert_eq!(endpoint.count(), 1);
    }
}

#[test]
fn http_deadline_bounds_public_endpoint() {
    for wrapped in [false, true] {
        let f = Fixture::new(true);
        let endpoint = Endpoint::new(200, Duration::from_millis(900), true);
        endpoint.configure(&f, 100, 4000);
        f.ctx
            .update_config(|config| config.search.gather.as_mut().unwrap().max_rounds = Some(1));
        let start = Instant::now();
        let response = f.search(true, None, QUERY, None, wrapped);
        assert!(
            start.elapsed() < Duration::from_millis(700),
            "HTTP request exceeded its deadline: {:?}",
            start.elapsed()
        );
        assert!(response.success);
        assert!(response.data["text"]
            .as_str()
            .unwrap()
            .contains("1:timeout"));
        assert_eq!(endpoint.count(), 1);
    }
}

#[test]
fn query_plus_pattern_keeps_ranked_prefix() {
    for wrapped in [false, true] {
        let f = Fixture::new(true);
        let endpoint = Endpoint::new(200, Duration::ZERO, true);
        endpoint.configure(&f, 2000, 6000);
        let plain = f.search(false, None, QUERY, Some("load_config"), wrapped);
        let response = f.search(
            true,
            Some("host-trace"),
            QUERY,
            Some("load_config"),
            wrapped,
        );
        assert_ranked_unchanged(&plain, &response);
        assert_eq!(endpoint.count(), 1);
    }
}

#[test]
fn denied_neighbour_never_reaches_endpoint_or_render() {
    for wrapped in [false, true] {
        let f = Fixture::new(true);
        let root = f.ctx.config().project_root.clone().unwrap();
        let main = root.join("main.ts");
        let denied = root.join("denied.ts");
        let source = "import { private_neighbour } from './denied';\n/** Load application config settings. */\nexport function load_config() { private_neighbour(); }\n";
        std::fs::write(&main, source).unwrap();
        std::fs::write(
            &denied,
            "export function private_neighbour() { const private_body_marker_a82d = 1; }\n",
        )
        .unwrap();
        let key = aft::search_index::artifact_cache_key(&root);
        let dir = f._temp.path().join("denied-graph").join(key);
        let (_writer, _) = CallGraphStore::cold_build_with_lease(
            dir.clone(),
            root.clone(),
            &[main.clone(), denied.clone()],
        )
        .unwrap();
        *f.ctx.callgraph_store().write().unwrap() = Some(Arc::new(
            CallGraphStore::open_readonly(dir, root).unwrap().unwrap(),
        ));
        let mut index = SearchIndex::new();
        index.index_file(&main, source.as_bytes());
        index.ready = true;
        *f.ctx.search_index().write().unwrap() = Some(index);
        let endpoint = Endpoint::new(503, Duration::ZERO, false);
        endpoint.configure(&f, 2000, 6000);
        let allowed = f.search(true, None, QUERY, None, wrapped);
        assert!(
            allowed.data["text"]
                .as_str()
                .unwrap()
                .contains("private_body_marker_a82d"),
            "fixture must reach its neighbour before denial: {}",
            allowed.data["text"]
        );
        endpoint.requests.lock().unwrap().clear();
        f.ctx
            .update_config(|cfg| cfg.sandbox.read_deny.push(denied.clone()));
        #[cfg(target_os = "linux")]
        let read_probe = SourceReadProbe::new(&denied);
        let response = f.search(true, None, QUERY, None, wrapped);
        assert!(response.success);
        #[cfg(target_os = "linux")]
        assert!(!read_probe.observed(), "denied neighbour source was read");
        assert!(
            !response.data["text"]
                .as_str()
                .unwrap()
                .contains("private_body_marker_a82d"),
            "denied neighbour was rendered"
        );
        assert!(
            !endpoint
                .requests
                .lock()
                .unwrap()
                .iter()
                .any(|request| request.to_string().contains("private_body_marker_a82d")),
            "denied neighbour reached the endpoint"
        );
    }
}

#[cfg(target_os = "linux")]
struct SourceReadProbe(std::os::fd::OwnedFd);

#[cfg(target_os = "linux")]
impl SourceReadProbe {
    fn new(path: &std::path::Path) -> Self {
        use std::os::fd::{AsRawFd, FromRawFd};
        use std::os::unix::ffi::OsStrExt;
        let path_bytes = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        assert!(fd >= 0, "inotify init: {}", std::io::Error::last_os_error());
        let probe = Self(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) });
        let watch = unsafe {
            libc::inotify_add_watch(probe.0.as_raw_fd(), path_bytes.as_ptr(), libc::IN_ACCESS)
        };
        assert!(
            watch >= 0,
            "inotify watch: {}",
            std::io::Error::last_os_error()
        );
        std::fs::read(path).unwrap();
        assert!(
            probe.observed(),
            "source read probe must observe its positive control"
        );
        probe
    }

    fn observed(&self) -> bool {
        use std::os::fd::AsRawFd;
        let mut bytes = [0u8; 4096];
        let count =
            unsafe { libc::read(self.0.as_raw_fd(), bytes.as_mut_ptr().cast(), bytes.len()) };
        if count >= 0 {
            count > 0
        } else {
            assert_eq!(
                std::io::Error::last_os_error().kind(),
                std::io::ErrorKind::WouldBlock
            );
            false
        }
    }
}

fn assert_ranked_unchanged(plain: &Response, gathered: &Response) {
    assert!(plain.success && gathered.success);
    assert!(!plain.data["results"].as_array().unwrap().is_empty());
    for key in [
        "results",
        "ranked_files",
        "results_list_envelope",
        "next_offset",
        "execution",
    ] {
        assert_eq!(plain.data[key], gathered.data[key], "changed {key}");
    }
    let mut ranked = plain.data.clone();
    let mut attached = gathered.data.clone();
    ranked.as_object_mut().unwrap().remove("text");
    attached.as_object_mut().unwrap().remove("text");
    assert_eq!(
        ranked, attached,
        "gather must not change ranked fields or metadata"
    );
    let plain_text = plain.data["text"].as_str().unwrap();
    let gathered_text = gathered.data["text"].as_str().unwrap();
    assert_eq!(
        gathered_text.matches("gather:").count(),
        1,
        "gather disclosure must appear exactly once: {gathered_text}"
    );
    assert!(
        gathered_text
            .matches("Related code (gathered for this search")
            .count()
            <= 1,
        "gather section must not repeat: {gathered_text}"
    );
    let suffix_start = plain_text
        .find("\nSearch status:")
        .or_else(|| plain_text.find("\n\nshown "))
        .unwrap_or(plain_text.len());
    let (body, suffix) = plain_text.split_at(suffix_start);
    assert!(gathered_text.starts_with(body), "ranked body changed");
    assert!(
        gathered_text.ends_with(suffix),
        "status and ranked-list trailer changed"
    );
}

#[test]
fn gather_search_test_prose_appends_without_changing_ranked_records() {
    for pattern in [None, Some("load_config")] {
        for trace in [None, Some(""), Some("private-trace-marker")] {
            let f = Fixture::new(true);
            let plain = f.search(
                false,
                None,
                "how is application config loaded",
                pattern,
                false,
            );
            let gathered = f.search(
                true,
                trace,
                "how is application config loaded",
                pattern,
                false,
            );
            assert_ranked_unchanged(&plain, &gathered);
            let text = gathered.data["text"].as_str().unwrap();
            assert!(
                text.contains("Related code (gathered for this search,"),
                "{text}"
            );
            assert!(text.contains("gather: model=deterministic"));
            assert!(
                text.contains("parse_config: function"),
                "must walk beyond the ranked seed: {text}"
            );
            assert!(text.contains(if trace.is_some_and(|t| !t.is_empty()) {
                "context: host trace"
            } else {
                "context: query only"
            }));
            assert!(!text.contains("private-trace-marker"));
        }
    }
}

#[test]
fn gather_search_test_tool_call_keeps_footer_and_context_source() {
    let f = Fixture::new(true);
    let response = f.search(
        true,
        Some("private-trace-marker"),
        "how is application config loaded",
        None,
        true,
    );
    assert!(response.success);
    let text = response.data["text"].as_str().unwrap();
    assert!(
        text.contains("Related code (gathered for this search,"),
        "{text}"
    );
    assert!(text.contains("context: host trace"));
    assert!(!text.contains("private-trace-marker"));
}

#[test]
fn gather_search_test_disabled_and_non_prose_do_not_walk() {
    let f = Fixture::new(true);
    for (query, pattern) in [
        ("load_config", None),
        ("config.rs", None),
        ("load_.*", None),
        ("\"load application\"", None),
        ("", Some("load_config")),
    ] {
        let response = f.search(true, Some("private-trace-marker"), query, pattern, false);
        assert!(response.success);
        assert!(
            !response.data["text"]
                .as_str()
                .unwrap_or_default()
                .contains("gather:"),
            "query={query} pattern={pattern:?}: {}",
            response.data["text"]
        );
    }
    let response = f.search(
        false,
        Some("private-trace-marker"),
        "how is application config loaded",
        None,
        false,
    );
    assert!(!response.data["text"].as_str().unwrap().contains("gather:"));
}

#[test]
fn gather_search_test_off_store_discloses_once_without_losing_results() {
    let f = Fixture::new(false);
    let plain = f.search(false, None, "how is application config loaded", None, false);
    let gathered = f.search(true, None, "how is application config loaded", None, false);
    assert_ranked_unchanged(&plain, &gathered);
    let text = gathered.data["text"].as_str().unwrap();
    assert_eq!(
        text.matches("gather: skipped callgraph=off").count(),
        1,
        "{text}"
    );
}

#[test]
fn gather_search_test_failed_endpoint_uses_disclosed_fallback() {
    let f = Fixture::new(true);
    f.ctx.update_config(|config| {
        config.search.gather = Some(GatherConfig {
            enabled: Some(true),
            decision: Some(GatherDecisionConfig {
                backend: Some(GatherDecisionBackend::Endpoint),
                endpoint: Some("not-a-url".into()),
                model: Some("fixture".into()),
                ..GatherDecisionConfig::default()
            }),
            ..GatherConfig::default()
        })
    });
    let req = serde_json::from_value(json!({"id":"fallback", "command":"semantic_search", "query":"how is application config loaded"})).unwrap();
    let response = handle_semantic_search(&req, &f.ctx);
    assert!(response.success);
    let text = response.data["text"].as_str().unwrap();
    assert!(text.contains("configuration_failure"), "{text}");
    assert!(text.contains("Related code (gathered for this search,"));
}

#[test]
fn all_non_ready_states_keep_plain_results() {
    for state in ["off", "building", "unavailable", "suspended", "error"] {
        let f = Fixture::new(false);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        f.ctx.update_config(|config| {
            config.indexes.callgraph = state != "off";
            config.search.gather = Some(GatherConfig {
                decision: Some(GatherDecisionConfig {
                    backend: Some(GatherDecisionBackend::Endpoint),
                    endpoint: Some(format!("http://{}/v1", listener.local_addr().unwrap())),
                    model: Some("fixture".into()),
                    ..GatherDecisionConfig::default()
                }),
                decision_timeout_ms: Some(20),
                deadline_ms: Some(100),
                ..GatherConfig::default()
            });
        });
        let (sender, receiver) = crossbeam_channel::bounded(1);
        match state {
            "building" => *f.ctx.callgraph_store_rx().lock() = Some(receiver),
            "unavailable" => f.ctx.set_heavy_root_work_allowed(false),
            "suspended" => f.ctx.record_callgraph_store_build_suspension(
                f.ctx.configure_generation(),
                aft::build_breaker::BuildSuspension {
                    domain: aft::build_breaker::BuildDomain::CallgraphCold,
                    reason: "private-reason-marker".into(),
                    death_count: 3,
                    suspended_since_unix_ms: 0,
                },
            ),
            "error" => f.ctx.record_callgraph_store_build_denied(
                f.ctx.configure_generation(),
                "private-reason-marker".into(),
            ),
            _ => {}
        }
        let plain = f.search(false, None, "how is application config loaded", None, false);
        let gathered = f.search(true, None, "how is application config loaded", None, false);
        assert_ranked_unchanged(&plain, &gathered);
        let text = gathered.data["text"].as_str().unwrap();
        assert_eq!(
            text.matches(&format!("gather: skipped callgraph={state}"))
                .count(),
            1,
            "{text}"
        );
        assert!(!text.contains("private-reason-marker"));
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock,
            "state={state}"
        );
        drop(sender);
    }
}

fn subc_gather_dispatch(req: RawRequest, ctx: &AppContext) -> Response {
    if req.command == "configure" {
        let response = super::subc_bridge_test::bridge_dispatch(req.clone(), ctx);
        let root = std::path::PathBuf::from(req.params["project_root"].as_str().unwrap());
        let config: GatherConfig =
            serde_json::from_slice(&std::fs::read(root.join("gather-fixture.json")).unwrap())
                .unwrap();
        ctx.update_config(|cfg| {
            cfg.views.enabled = false;
            cfg.indexes.callgraph = true;
            cfg.search.gather = Some(config);
        });
        let file = root.join("main.rs");
        let source = std::fs::read(&file).unwrap();
        let mut index = SearchIndex::new();
        index.index_file(&file, &source);
        index.ready = true;
        *ctx.search_index().write().unwrap() = Some(index);
        let key = aft::search_index::artifact_cache_key(&root);
        aft::root_cache::configure_artifact_access(&root, &key, false);
        let dir = root.join("graph").join(key);
        let (_writer, _) =
            CallGraphStore::cold_build_with_lease(dir.clone(), root.clone(), &[file]).unwrap();
        *ctx.callgraph_store().write().unwrap() = Some(Arc::new(
            CallGraphStore::open_readonly(dir, root).unwrap().unwrap(),
        ));
        return response;
    }
    if req.command == "semantic_search" {
        handle_semantic_search(&req, ctx)
    } else {
        super::subc_bridge_test::bridge_dispatch(req, ctx)
    }
}

#[test]
fn subc_endpoint_fallback_and_deadlines() {
    use super::subc_bridge_test::*;
    for case in ["fallback", "http", "walk"] {
        run_subc_bridge_test_with_dispatch(
            "gather-subc-fallback-deadlines",
            Duration::from_secs(30),
            move |input| async move {
                let mut session = open_fake_daemon_session(input).await;
                let endpoint = Endpoint::new(
                    503,
                    if case == "fallback" {
                        Duration::ZERO
                    } else {
                        Duration::from_millis(900)
                    },
                    false,
                );
                std::fs::write(session.root1.join("main.rs"), SOURCE).unwrap();
                std::fs::write(session.root1.join("gather-fixture.json"), json!({
                    "enabled":true, "decision":{"backend":"endpoint","endpoint":endpoint.url,"model":"boundary-fixture"},
                    "decision_timeout_ms":if case == "http" {100} else {2000},
                    "deadline_ms":if case == "walk" {100} else {6000},
                    "max_rounds":if case == "http" {1} else {3}
                }).to_string()).unwrap();
                gather_bind_route(
                    &mut session.stream,
                    &session.root1,
                    1,
                    1,
                    10,
                    "opencode",
                    "a",
                )
                .await;
                let start = Instant::now();
                let reply =
                    gather_search_frame(&mut session.stream, 1, 1, 11, Some("subc-trace")).await;
                let elapsed = start.elapsed();
                assert_eq!(reply["isError"], false, "{reply}");
                let text = reply["content"][0]["text"].as_str().unwrap();
                if case == "fallback" {
                    assert!(
                        text.contains("parse_config: function"),
                        "subc fallback lost neighbour evidence: {text}"
                    );
                    assert!(text.contains("1:unavailable"), "{text}");
                } else {
                    assert!(
                        elapsed < Duration::from_millis(700),
                        "subc {case} deadline exceeded: {elapsed:?}"
                    );
                    assert!(text.contains("1:timeout"), "{text}");
                    if case == "walk" {
                        assert!(text.contains("stop=deadline"), "{text}");
                    }
                    assert_eq!(endpoint.count(), 1);
                }
                send_connection_goodbye(&mut session.stream).await;
            },
            |_, _, _| {},
            subc_gather_dispatch,
        );
    }
}

#[test]
fn subc_public_endpoint_cache_context_and_untrusted_boundary() {
    use super::subc_bridge_test::*;
    run_subc_bridge_test_with_dispatch(
        "gather-public-boundaries",
        Duration::from_secs(30),
        |input| async move {
            let mut session = open_fake_daemon_session(input).await;
            let endpoint = Endpoint::new(200, Duration::ZERO, true);
            std::fs::write(session.root1.join("main.rs"), SOURCE).unwrap();
            std::fs::write(session.root1.join("gather-fixture.json"), json!({
                "enabled":true, "decision":{"backend":"endpoint","endpoint":endpoint.url,"model":"boundary-fixture"},
                "decision_timeout_ms":2000,"deadline_ms":6000
            }).to_string()).unwrap();
            gather_bind_route(
                &mut session.stream,
                &session.root1,
                1,
                1,
                10,
                "opencode",
                "a",
            )
            .await;
            gather_bind_route(&mut session.stream, &session.root1, 2, 1, 20, "pi", "b").await;
            let trace = format!("discard{}🙂", "界".repeat(5999));
            for corr in [11, 12] {
                let reply =
                    gather_search_frame(&mut session.stream, 1, 1, corr, Some(&trace)).await;
                assert_eq!(reply["isError"], false, "{reply}");
                assert!(
                    reply["content"][0]["text"]
                        .as_str()
                        .unwrap()
                        .contains("model=boundary-fixture"),
                    "{reply}"
                );
            }
            assert_eq!(endpoint.count(), 1);
            let sent = endpoint.requests.lock().unwrap()[0]["trace"]
                .as_str()
                .unwrap()
                .to_owned();
            assert_eq!(sent.chars().count(), 6000);
            assert!(!sent.contains("discard"));
            let reply = gather_search_frame(&mut session.stream, 2, 1, 21, Some(&trace)).await;
            assert_eq!(reply["isError"], false, "{reply}");
            assert_eq!(
                endpoint.count(),
                2,
                "two sessions on one root require separate decisions"
            );
            gather_bind_route(
                &mut session.stream,
                &session.root1,
                3,
                1,
                30,
                "mcp:gather-fixture",
                "untrusted",
            )
            .await;
            let reply =
                gather_search_frame(&mut session.stream, 3, 1, 31, Some("untrusted-marker-28f0"))
                    .await;
            assert_eq!(reply["isError"], false, "{reply}");
            assert!(
                reply["content"][0]["text"]
                    .as_str()
                    .unwrap()
                    .contains("context: query only"),
                "{reply}"
            );
            assert_eq!(endpoint.count(), 3);
            assert!(
                endpoint.requests.lock().unwrap().last().unwrap()["trace"].is_null(),
                "untrusted context reached the walk"
            );
            send_connection_goodbye(&mut session.stream).await;
        },
        |_, _, _| {},
        subc_gather_dispatch,
    );
}

#[test]
fn trace_request_and_key_never_reach_logs() {
    const CHILD: &str = "AFT_GATHER_PUBLIC_LOG_CHILD";
    const TRACE: &str = "public_trace_marker_839ab";
    const BODY: &str = "public_body_marker_129ce";
    const KEY: &str = "public_key_marker_9aa4c";
    const ENV: &str = "AFT_GATHER_PUBLIC_FAKE_KEY";
    if std::env::var_os(CHILD).is_some() {
        env_logger::Builder::new()
            .filter_level(log::LevelFilter::Trace)
            .init();
        log::info!("gather-public-log-anchor");
        for wrapped in [false, true] {
            let f = Fixture::new(true);
            let root = f.ctx.config().project_root.clone().unwrap();
            let source = SOURCE.replace(
                "fn parse_config()",
                &format!("/// {BODY}\nfn parse_config()"),
            );
            std::fs::write(root.join("main.rs"), source).unwrap();
            let endpoint = Endpoint::new(503, Duration::ZERO, false);
            endpoint.configure(&f, 2000, 6000);
            f.ctx.update_config(|config| {
                config
                    .search
                    .gather
                    .as_mut()
                    .unwrap()
                    .decision
                    .as_mut()
                    .unwrap()
                    .api_key_env = Some(ENV.into())
            });
            let response = f.search(true, Some(TRACE), QUERY, None, wrapped);
            assert!(response.success);
            assert!(!endpoint.requests.lock().unwrap().is_empty());
            assert!(endpoint
                .requests
                .lock()
                .unwrap()
                .iter()
                .any(|request| request.to_string().contains(TRACE)));
            assert!(endpoint
                .requests
                .lock()
                .unwrap()
                .iter()
                .any(|request| request.to_string().contains(BODY)));
        }
        subc_public_endpoint_cache_context_and_untrusted_boundary();
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "gather_search_test::trace_request_and_key_never_reach_logs",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .env(ENV, KEY)
        .output()
        .unwrap();
    std::fs::write(temp.path().join("stdout.log"), &output.stdout).unwrap();
    std::fs::write(temp.path().join("stderr.log"), &output.stderr).unwrap();
    assert!(
        output.status.success(),
        "isolated log test failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let logs = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(logs.contains("gather-public-log-anchor"));
    assert!(logs.contains("1 passed"));
    for marker in [TRACE, BODY, KEY, "untrusted-marker-28f0"] {
        assert!(
            !logs.contains(marker),
            "private marker reached fixture-local logs"
        );
    }
}

#[test]
fn gather_search_test_no_code_seeds_discloses_without_decision() {
    let f = Fixture::new(true);
    let file = f
        .ctx
        .config()
        .project_root
        .as_ref()
        .unwrap()
        .join("notes.md");
    let source = "how is documentation loaded\n";
    std::fs::write(&file, source).unwrap();
    let mut index = SearchIndex::new();
    index.index_file(&file, source.as_bytes());
    index.ready = true;
    *f.ctx.search_index().write().unwrap() = Some(index);
    let plain = f.search(false, None, "how is documentation loaded", None, false);
    let gathered = f.search(true, None, "how is documentation loaded", None, false);
    assert_ranked_unchanged(&plain, &gathered);
    assert!(gathered.data["text"]
        .as_str()
        .unwrap()
        .contains("gather: skipped callgraph=no_seeds"));
}

#[test]
fn gather_search_test_external_root_never_uses_current_graph() {
    let f = Fixture::new(true);
    let root = f._temp.path().join("external");
    std::fs::create_dir(&root).unwrap();
    let mut git = std::process::Command::new("git");
    crate::test_helpers::apply_hermetic_git_env(&mut git);
    assert!(git
        .current_dir(&root)
        .args(["init", "--quiet"])
        .status()
        .unwrap()
        .success());
    std::fs::write(root.join("main.rs"), SOURCE).unwrap();
    f.ctx.update_config(|config| {
        config.search.gather = Some(GatherConfig {
            enabled: Some(true),
            ..GatherConfig::default()
        })
    });
    let req = serde_json::from_value(json!({"id":"external-gather", "command":"semantic_search", "query":"how is application config loaded", "pattern":"load_config", "path":root})).unwrap();
    let response = handle_semantic_search(&req, &f.ctx);
    assert!(response.success, "{}", response.data);
    let text = response.data["text"].as_str().unwrap();
    assert!(
        text.contains("gather: skipped callgraph=unavailable"),
        "{text}"
    );
    assert!(text.contains("reason=other_project"), "{text}");
    assert!(!text.contains("Related code (gathered"));
}
