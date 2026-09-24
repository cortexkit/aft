//! Runtime responses of every index consumer, compared with the shared frozen
//! fixtures in `spec/feature-config/consumer-responses.json`.
//!
//! Each fixture row is a projection: only the keys the row lists are compared,
//! and a key the runtime omits compares as `null`. Status-bearing fields
//! (`index`, `fallback`, `code`, `lanes`, `omitted_lanes`, `enrichment`,
//! `annotations` and the dead-code analysis) are compared literally. Ordinary
//! result rows coexist with those fields; where the fixture's illustrative
//! row shape differs from the engine's own result format (grep/glob `text`,
//! aft_search result rows, the todos analysis) the test checks that the
//! ordinary result is present instead of comparing it literally.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::context::{AppContext, SemanticIndexStatus};
use crate::feature_status::{observed_index_status, IndexPlane};
use crate::parser::TreeSitterProvider;
use crate::protocol::{RawRequest, Response};

fn fixture_rows() -> Vec<Value> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../spec/feature-config/consumer-responses.json");
    let doc: Value = serde_json::from_str(
        &std::fs::read_to_string(&path).expect("read shared consumer fixtures"),
    )
    .expect("parse shared consumer fixtures");
    doc["fixture_rows"]
        .as_array()
        .expect("fixture_rows")
        .clone()
}

fn expected(consumer: &str, scenario: &str) -> Value {
    fixture_rows()
        .into_iter()
        .find(|row| row["consumer"] == consumer && row["scenario"] == scenario)
        .unwrap_or_else(|| panic!("missing fixture {consumer}/{scenario}"))["expected"]
        .clone()
}

const SCENARIOS: [&str; 4] = ["off", "building", "unavailable", "partially_ready"];

/// Everything a scenario keeps alive for the duration of one consumer call:
/// the project directory and the senders behind simulated in-flight builds.
struct Scenario {
    ctx: AppContext,
    root: tempfile::TempDir,
    _search_tx: Option<crossbeam_channel::Sender<crate::search_index::SearchIndex>>,
    _callgraph_tx: Option<crossbeam_channel::Sender<crate::context::CallGraphStoreBuildEvent>>,
}

impl Scenario {
    fn root(&self) -> PathBuf {
        std::fs::canonicalize(self.root.path()).expect("canonical root")
    }
}

/// Build a context whose index planes are in the named scenario:
/// - `off`: every index configured off;
/// - `building`: every enabled index has a build in flight;
/// - `unavailable`: every index enabled but never observed (callgraph reads
///   are borrow-only so a query cannot start a build);
/// - `partially_ready`: trigram ready, semantic backend down, callgraph
///   building.
fn scenario(name: &str, files: &[(&str, &str)]) -> Scenario {
    let root = tempfile::tempdir().expect("project root");
    for (rel, content) in files {
        let path = root.path().join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
    }
    let project_root = std::fs::canonicalize(root.path()).unwrap();
    let ctx = AppContext::new(
        Box::new(TreeSitterProvider::new()),
        crate::config::Config {
            project_root: Some(project_root.clone()),
            ..crate::config::Config::default()
        },
    );
    let mut search_tx = None;
    let mut callgraph_tx = None;
    match name {
        "off" => ctx.update_config(|config| {
            config.indexes.trigram = false;
            config.indexes.semantic = false;
            config.indexes.callgraph = false;
        }),
        "building" => {
            let (tx, rx) = crossbeam_channel::unbounded();
            *ctx.search_index_rx().write().unwrap() = Some(rx);
            search_tx = Some(tx);
            *ctx.semantic_index_status().write().unwrap() = SemanticIndexStatus::Building {
                stage: "embedding".to_string(),
                files: None,
                entries_done: None,
                entries_total: None,
            };
            let (tx, rx) = crossbeam_channel::unbounded();
            *ctx.callgraph_store_rx().lock() = Some(rx);
            callgraph_tx = Some(tx);
        }
        "unavailable" => ctx.set_cache_writer_capabilities(false, true),
        "partially_ready" => {
            let index = crate::search_index::SearchIndex::build(&project_root);
            assert!(index.ready, "a built search index is ready");
            *ctx.search_index().write().unwrap() = Some(index);
            ctx.trip_semantic_refresh_circuit(1, "embedding backend refused the connection");
            let (tx, rx) = crossbeam_channel::unbounded();
            *ctx.callgraph_store_rx().lock() = Some(rx);
            callgraph_tx = Some(tx);
        }
        other => panic!("unknown scenario {other}"),
    }
    Scenario {
        ctx,
        root,
        _search_tx: search_tx,
        _callgraph_tx: callgraph_tx,
    }
}

fn request(command: &str, params: Value) -> RawRequest {
    RawRequest {
        id: format!("fixture-{command}"),
        command: command.to_string(),
        lsp_hints: None,
        session_id: None,
        params,
    }
}

fn response_json(response: &Response) -> Value {
    let mut value = response.data.clone();
    value["success"] = json!(response.success);
    value
}

/// Keep only the keys the fixture lists; an omitted runtime key reads `null`.
fn project(actual: &Value, expected: &Value) -> Value {
    let mut out = serde_json::Map::new();
    for key in expected.as_object().expect("fixture object").keys() {
        out.insert(key.clone(), actual.get(key).cloned().unwrap_or(Value::Null));
    }
    Value::Object(out)
}

fn relative(root: &Path, path: &str) -> String {
    Path::new(path)
        .strip_prefix(root)
        .unwrap_or(Path::new(path))
        .display()
        .to_string()
}

const NEEDLE_FILE: (&str, &str) = ("src/example.ts", "needle\n");

#[test]
fn grep_matches_the_shared_fixtures() {
    for name in SCENARIOS {
        let expected = expected("grep", name);
        let scenario = scenario(name, &[NEEDLE_FILE]);
        let root = scenario.root();
        let response = crate::commands::grep::handle_grep(
            &request("grep", json!({ "pattern": "needle" })),
            &scenario.ctx,
        );
        let mut actual = response_json(&response);
        for grep_match in actual["matches"].as_array_mut().expect("matches") {
            let file = relative(&root, grep_match["file"].as_str().unwrap());
            grep_match["file"] = json!(file);
        }
        let text = actual["text"].as_str().expect("text").to_string();
        let mut projected = project(&actual, &expected);
        let mut frozen = expected.clone();
        projected.as_object_mut().unwrap().remove("text");
        frozen.as_object_mut().unwrap().remove("text");
        assert_eq!(projected, frozen, "grep/{name}");
        // The ordinary result coexists with the disclosure: the walk found the
        // match whatever the index state.
        assert!(text.contains("needle"), "grep/{name} text: {text}");
        assert!(
            text.contains("Found 1 match across 1 file"),
            "grep/{name} text: {text}"
        );
    }
}

#[test]
fn glob_matches_the_shared_fixtures() {
    for name in SCENARIOS {
        let expected = expected("glob", name);
        let scenario = scenario(name, &[NEEDLE_FILE]);
        let root = scenario.root();
        let response = crate::commands::glob::handle_glob(
            &request("glob", json!({ "pattern": "**/*.ts" })),
            &scenario.ctx,
        );
        let mut actual = response_json(&response);
        let files = actual["files"]
            .as_array()
            .expect("files")
            .iter()
            .map(|file| json!(relative(&root, file.as_str().unwrap())))
            .collect::<Vec<_>>();
        actual["files"] = json!(files);
        let text = actual["text"].as_str().expect("text").to_string();
        let mut projected = project(&actual, &expected);
        let mut frozen = expected.clone();
        projected.as_object_mut().unwrap().remove("text");
        frozen.as_object_mut().unwrap().remove("text");
        assert_eq!(projected, frozen, "glob/{name}");
        assert!(text.contains("example.ts"), "glob/{name} text: {text}");
    }
}

fn search(scenario: &Scenario) -> Value {
    response_json(&crate::commands::semantic_search::handle_semantic_search(
        &request("semantic_search", json!({ "query": "needle" })),
        &scenario.ctx,
    ))
}

#[test]
fn aft_search_matches_the_shared_fixtures() {
    for name in SCENARIOS {
        let expected = expected("aft_search", name);
        let scenario = scenario(name, &[NEEDLE_FILE]);
        let actual = search(&scenario);
        if name == "partially_ready" {
            // Ranked result rows use the engine's own row format; the
            // fixture pins the lane labelling that coexists with them.
            assert_eq!(
                actual["success"],
                json!(true),
                "aft_search/{name}: {actual:#}"
            );
            assert!(actual["code"].is_null());
            assert_eq!(actual["lanes"], expected["lanes"], "aft_search/{name}");
            assert_eq!(
                actual["omitted_lanes"], expected["omitted_lanes"],
                "aft_search/{name}"
            );
            assert!(
                actual["text"]
                    .as_str()
                    .is_some_and(|text| text.contains("example.ts")),
                "aft_search/{name} must still return the lexical result: {actual:#}"
            );
        } else {
            assert_eq!(project(&actual, &expected), expected, "aft_search/{name}");
        }
    }
}

#[test]
fn aft_search_callgraph_enrichment_matches_the_shared_fixtures() {
    for name in SCENARIOS {
        let expected = expected("aft_search.callgraph_enrichment", name);
        let scenario = scenario(name, &[NEEDLE_FILE]);
        let actual = search(&scenario);
        assert_eq!(
            actual["enrichment"], expected["enrichment"],
            "enrichment/{name}"
        );
        assert_eq!(actual["lanes"], expected["lanes"], "enrichment/{name}");
        assert_eq!(actual["success"], expected["success"], "enrichment/{name}");
        if name != "partially_ready" {
            assert_eq!(actual["results"], expected["results"], "enrichment/{name}");
        }
    }
}

const EXAMPLE_FUNCTION: (&str, &str) = ("src/example.ts", "function example() {}\n");

#[test]
fn aft_callgraph_matches_the_shared_fixtures() {
    for name in SCENARIOS {
        let expected = expected("aft_callgraph", name);
        let scenario = scenario(name, &[EXAMPLE_FUNCTION]);
        let file = scenario.root().join("src/example.ts");
        let response = crate::commands::callers::handle_callers(
            &request("callers", json!({ "file": file, "symbol": "example" })),
            &scenario.ctx,
        );
        let actual = response_json(&response);
        assert_eq!(
            project(&actual, &expected),
            expected,
            "aft_callgraph/{name}"
        );
        assert!(
            scenario.ctx.callgraph_store().read().unwrap().is_none(),
            "aft_callgraph/{name}: a refusal must not install a store"
        );
    }
}

#[test]
fn aft_zoom_callgraph_matches_the_shared_fixtures() {
    for name in SCENARIOS {
        let expected = expected("aft_zoom.callgraph", name);
        let scenario = scenario(name, &[EXAMPLE_FUNCTION]);
        let file = scenario.root().join("src/example.ts");
        let response = crate::commands::zoom::handle_zoom(
            &request(
                "zoom",
                json!({ "file": file, "symbol": "example", "callgraph": true }),
            ),
            &scenario.ctx,
        );
        let actual = response_json(&response);
        assert_eq!(
            project(&actual, &expected),
            expected,
            "aft_zoom.callgraph/{name}"
        );
    }
}

#[test]
fn aft_inspect_dead_code_matches_the_shared_fixtures() {
    for name in SCENARIOS {
        let expected = expected("aft_inspect.dead_code", name);
        let scenario = scenario(name, &[EXAMPLE_FUNCTION]);
        let payload = crate::inspect::scanners::dead_code::callgraph_unavailable_aggregate(1);
        let mut summary = crate::commands::inspect::summary_for_test(
            crate::inspect::InspectCategory::DeadCode,
            &payload,
        );
        crate::commands::inspect::annotate_dead_code_unavailable_for_test(
            &mut summary,
            &payload,
            observed_index_status(&scenario.ctx, IndexPlane::Callgraph),
        );
        let frozen = &expected["analyses"]["dead_code"];
        assert_eq!(project(&summary, frozen), *frozen, "dead_code/{name}");
        // Never a zero count: the unavailable summary carries no count at all.
        assert!(
            summary.get("count").is_none(),
            "dead_code/{name}: {summary:#}"
        );
    }
}
