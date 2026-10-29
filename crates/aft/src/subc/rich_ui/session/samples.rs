//! Reproducible rich-UI replies captured through ToolProviderV1 `tool.call`
//! requests by `reserved:occipital`, the read-only UI carrier principal.

use super::*;

const FIXTURE: &str = "tests/fixtures/rich_ui/aft-rich-ui-manifest-v1.json";
const TASK: &str = "bash-0000000000000081";
const VIEWED_CALL: &str = "sample-viewed-call";

async fn capture() -> Value {
    // These calls inject a test manifest without publishing a production manifest.
    let _manifest =
        super::super::super::chunks::set_test_manifest_for_test(Some(MANIFEST.to_vec()));
    let f = read_tests::indexed_fixture("head");
    let mut vectors = Vec::new();
    for (id, args) in [
        (
            "read",
            json!({"path":"main.rs", "startLine":1, "endLine":2}),
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
        let body = read_tests::query_with(&f, id, args.clone(), read_tests::dispatch).await;
        assert_eq!(body["isError"], false, "{id}: {body}");
        vectors.push(vector(id, id, args, body));
    }

    bash_tests::seed(
        &f,
        TASK,
        "session",
        "reserved:prefrontal-core",
        VIEWED_CALL,
        false,
        false,
    );
    for (id, args) in [
        ("bash_status", json!({})),
        ("bash_output", json!({"limit":200})),
        ("bash_watch", json!({"pattern":"one", "timeoutMs":1})),
    ] {
        let body = bash_tests::query(&f, id, VIEWED_CALL, args.clone()).await;
        assert_eq!(body["isError"], false, "{id}: {body}");
        let mut args = args;
        args["task"] = VIEWED_CALL.into();
        vectors.push(vector(id, id, args, body));
    }

    let args = json!({"path":"missing.rs"});
    let body = read_tests::query_with(&f, "read", args.clone(), read_tests::dispatch).await;
    assert_eq!(body["isError"], true, "missing read: {body}");
    vectors.push(vector("read_missing", "read", args, body));

    let mut document = json!({
        "format":"aft-rich-ui-manifest-v1",
        "publication":"injected-test-manifest",
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
        let operation = super::super::super::admission::prepare(
            request["name"].as_str().unwrap(),
            &request["arguments"],
        )
        .unwrap();
        super::super::super::carrier::validate_body(Some(&operation.entry), &vector["reply"])
            .unwrap();
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
}
