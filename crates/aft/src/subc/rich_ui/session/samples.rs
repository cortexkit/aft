//! Reproducible rich-UI replies captured through ToolProviderV1 `tool.call`
//! requests by `reserved:occipital`, the read-only UI carrier principal.

use super::*;

const FIXTURE: &str = "tests/fixtures/rich_ui/aft-rich-ui-manifest-v1.json";
const TASK: &str = "bash-0000000000000081";
const VIEWED_CALL: &str = "sample-viewed-call";

fn sample_fixture() -> Fixture {
    read_tests::indexed_fixture_with_setup("head", |f| {
        // Keep the database, caches and task artifacts outside the watched
        // project so their writes cannot become new project-indexing work.
        let project = f.dir.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let project = project.canonicalize().unwrap();
        let mut config = f.ctx.config().clone();
        config.project_root = Some(project.clone());
        let db = f.ctx.db().unwrap();
        f.ctx = Arc::new(AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            config,
        ));
        f.ctx.set_db(db);
        for route in [&mut f.carrier, &mut f.policy] {
            let identity = Arc::get_mut(&mut route.0).unwrap();
            identity.root = ProjectRootId::from_path(&project).unwrap();
            identity.project_root = project.clone();
            if let AuthenticatedPrincipal::RouteBind { project_root, .. } =
                &mut identity.spawn_principal
            {
                *project_root = project.clone();
            }
        }
    })
}

fn wait_for_capture_ready(ctx: &AppContext, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        crate::runtime_drain::drain_watcher_events(ctx);
        crate::runtime_drain::drain_build_completions(ctx);
        let search_ready = ctx
            .search_index()
            .read()
            .unwrap()
            .as_ref()
            .is_some_and(|index| index.ready);
        let callgraph_ready = matches!(
            ctx.callgraph_store_for_ops(),
            crate::context::CallgraphStoreAccess::Ready(_)
        );
        // These are the same pending signals captured before indexed tool calls
        // and rendered as watcher/callgraph lag disclosures in their replies.
        let pending = ctx.pending_changes_for_query(true);
        let callgraph_pending = pending.callgraph.resolved();
        assert!(
            Instant::now() < deadline,
            "rich-UI sample fixture did not catch up within {timeout:?}: \
             search_ready={search_ready}, callgraph_ready={callgraph_ready}, \
             watcher_pending={}, callgraph_pending={callgraph_pending:?}",
            pending.watcher,
        );
        if search_ready && callgraph_ready && !pending.watcher && callgraph_pending.is_empty() {
            return;
        }
        std::thread::yield_now();
    }
}

async fn capture() -> Value {
    // These calls inject a test manifest without publishing a production manifest.
    let _manifest =
        super::super::super::chunks::set_test_manifest_for_test(Some(MANIFEST.to_vec()));
    let f = sample_fixture();
    // Finish task-artifact writes before waiting for any indexed sample reply.
    bash_tests::seed(
        &f,
        TASK,
        "session",
        "reserved:prefrontal-core",
        VIEWED_CALL,
        false,
        false,
    );
    let mut vectors = Vec::new();
    for (id, args) in [
        (
            "read",
            json!({"path":f.policy.project_root.join("main.rs"), "startLine":1, "endLine":2}),
        ),
        ("outline", json!({"target":"main.rs"})),
        (
            "zoom",
            json!({"batch":[{"target":"main.rs", "symbols":["session_marker"]}]}),
        ),
        ("search", json!({"pattern":"session_marker", "topK":3})),
        (
            "grep",
            json!({"pattern":"session_marker", "include":"main.rs"}),
        ),
        ("glob", json!({"pattern":"main.rs"})),
        (
            "callgraph",
            json!({"op":"callers", "path":"main.rs", "symbol":"session_leaf"}),
        ),
    ] {
        wait_for_capture_ready(&f.ctx, Duration::from_secs(60));
        let body = read_tests::query_with(&f, id, args.clone(), read_tests::dispatch).await;
        assert_eq!(body["isError"], false, "{id}: {body}");
        vectors.push(vector(id, id, args, body));
    }

    for (id, args) in [
        ("bash_status", json!({})),
        ("bash_output", json!({"limit":200})),
        ("bash_watch", json!({"pattern":"one", "timeoutMs":1})),
    ] {
        wait_for_capture_ready(&f.ctx, Duration::from_secs(60));
        let body = bash_tests::query(&f, id, VIEWED_CALL, args.clone()).await;
        assert_eq!(body["isError"], false, "{id}: {body}");
        let mut args = args;
        args["task"] = VIEWED_CALL.into();
        vectors.push(vector(id, id, args, body));
    }

    let args = json!({"path":"missing.rs"});
    wait_for_capture_ready(&f.ctx, Duration::from_secs(60));
    let body = read_tests::query_with(&f, "read", args.clone(), read_tests::dispatch).await;
    assert_eq!(body["isError"], true, "missing read: {body}");
    vectors.push(vector("read_missing", "read", args, body));

    let mut document = json!({
        "format":"aft-rich-ui-manifest-v1",
        "publication":"injected-test-manifest",
        "manifestDigest":super::super::super::canonical::manifest_digest(MANIFEST),
        "vectors":vectors,
    });
    normalize(&mut document, &f);
    document
}

fn vector(name: &str, id: &str, mut args: Value, body: Value) -> Value {
    args["rich_ui_op"] = format!("{id}@1").into();
    let tool = if id == "bash_output" {
        "bash_status"
    } else {
        id
    };
    json!({"name":name, "request":{"name":tool, "arguments":args}, "reply":body})
}

fn normalize(value: &mut Value, fixture: &Fixture) {
    let root = fixture.policy.project_root.to_string_lossy();
    normalize_value(value, &root);
}

fn normalize_value(value: &mut Value, root: &str) {
    match value {
        Value::Object(fields) => {
            for (key, value) in fields {
                match key.as_str() {
                    "started_at" | "finished_at" | "duration_ms" | "elapsed_ms" => {
                        if value.is_number() {
                            *value = json!(0);
                        }
                    }
                    _ => normalize_value(value, root),
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                normalize_value(value, root);
            }
        }
        Value::String(text) => {
            *text = text
                .replace(&format!("{root}\\"), "${FIXTURE_ROOT}/")
                .replace(root, "${FIXTURE_ROOT}")
                .replace(&root.replace('\\', "/"), "${FIXTURE_ROOT}")
                .replace(TASK, "bash-SAMPLE")
                .replace(VIEWED_CALL, "sample-CALL");
        }
        _ => {}
    }
}

fn encoded(document: &Value) -> Vec<u8> {
    let mut bytes = serde_json::to_vec_pretty(document).unwrap();
    bytes.push(b'\n');
    bytes
}

fn validate(document: &Value) {
    let vectors = document["vectors"].as_array().unwrap();
    let entries = super::super::super::operations::operations();
    for entry in &entries {
        let id = entry["id"].as_str().unwrap();
        let successes: Vec<_> = vectors
            .iter()
            .filter(|v| v["name"] == id && v["reply"]["isError"] == false)
            .collect();
        assert_eq!(successes.len(), 1, "one real-handler vector for {id}");
    }
    for vector in vectors {
        let request = &vector["request"];
        let (id, version) = request["arguments"]["rich_ui_op"]
            .as_str()
            .unwrap()
            .rsplit_once('@')
            .unwrap();
        assert_eq!(version, "1");
        let entry = entries.iter().find(|entry| entry["id"] == id).unwrap();
        assert_eq!(entry["target"]["tool"], request["name"]);
        super::super::super::carrier::validate_body(Some(entry), &vector["reply"]).unwrap();
    }
}

#[tokio::test]
async fn rich_ui_samples_match_handlers() {
    let document = capture().await;
    validate(&document);
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE);
    let expected = std::fs::read(&path).expect("committed rich-UI samples must exist");
    assert_eq!(
        String::from_utf8(encoded(&document)).unwrap(),
        String::from_utf8(expected).unwrap(),
        "real carrier replies drifted; regenerate with the command in WORKER.md"
    );
}

#[test]
fn rich_ui_samples_wait_for_pending_watcher_application() {
    let f = sample_fixture();
    wait_for_capture_ready(&f.ctx, Duration::from_secs(60));
    // Replace OS event delivery with a queued real path application so the
    // assertion does not depend on when a platform's watcher reports a write.
    f.ctx.stop_watcher_runtime();
    let (watcher_tx, watcher_rx) = crossbeam_channel::unbounded();
    *f.ctx.watcher_rx().lock() = Some(watcher_rx);
    let path = f.policy.project_root.join("main.rs");
    let (reached_rx, release_tx) =
        crate::runtime_drain::install_watcher_phase_commit_gate_for_test(path.clone());
    watcher_tx
        .send(crate::watcher_filter::WatcherDispatchEvent::Paths(vec![
            path,
        ]))
        .unwrap();
    let ctx = f.ctx.clone();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let waiter = std::thread::spawn(move || {
        wait_for_capture_ready(&ctx, Duration::from_secs(60));
        done_tx.send(()).unwrap();
    });
    let reached = reached_rx.recv_timeout(Duration::from_secs(10));
    let pending = f.ctx.watcher_query_has_pending_changes();
    let early_result = done_rx.try_recv();
    // Release and join even if the observations below fail, so no drain thread
    // or path-scoped test gate can outlive the fixture.
    let _ = release_tx.send(());
    waiter.join().unwrap();
    reached.expect("sample-readiness barrier must apply the queued watcher event");
    assert!(pending, "the held watcher application must remain pending");
    assert_eq!(
        early_result,
        Err(std::sync::mpsc::TryRecvError::Empty),
        "sample-readiness barrier returned before the watcher applied the event"
    );
    assert!(!f.ctx.watcher_query_has_pending_changes());
    assert!(f
        .ctx
        .pending_changes_for_query(true)
        .callgraph
        .resolved()
        .is_empty());
}

#[test]
#[should_panic(expected = "search_ready=false, callgraph_ready=false, watcher_pending=false")]
fn rich_ui_samples_readiness_timeout_names_pending_state() {
    let f = Fixture::new("head");
    wait_for_capture_ready(&f.ctx, Duration::ZERO);
}

#[tokio::test]
#[ignore = "explicitly regenerates the committed rich-UI samples"]
async fn regenerate_rich_ui_samples() {
    let document = capture().await;
    validate(&document);
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE);
    std::fs::write(&path, encoded(&document)).unwrap();
    println!(
        "RICH_UI_SAMPLES_GENERATED {} vectors: {}",
        document["vectors"].as_array().unwrap().len(),
        path.display()
    );
}

#[test]
fn rich_ui_samples_normalize_only_volatile_values() {
    let mut value = json!({
        "text":"/random/project/main.rs bash-0000000000000081 sample-viewed-call",
        "started_at":1234, "finished_at":5678, "duration_ms":12,
        "waited":{"elapsed_ms":9, "limit_ms":20000, "reason":"exit"},
        "exitCode":42, "bytes":4, "running":false, "next":null,
    });
    normalize_value(&mut value, "/random/project");
    assert_eq!(
        value,
        json!({
            "text":"${FIXTURE_ROOT}/main.rs bash-SAMPLE sample-CALL",
            "started_at":0, "finished_at":0, "duration_ms":0,
            "waited":{"elapsed_ms":0, "limit_ms":20000, "reason":"exit"},
            "exitCode":42, "bytes":4, "running":false, "next":null,
        })
    );

    let mut windows_paths = json!([r"C:\fixture\main.rs", "C:/fixture/main.rs"]);
    normalize_value(&mut windows_paths, r"C:\fixture");
    assert_eq!(
        windows_paths,
        json!(["${FIXTURE_ROOT}/main.rs", "${FIXTURE_ROOT}/main.rs"])
    );
}
