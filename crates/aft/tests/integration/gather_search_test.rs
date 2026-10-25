use aft::callgraph_store::CallGraphStore;
use aft::commands::{semantic_search::handle_semantic_search, tool_call};
use aft::config::{Config, GatherConfig, GatherDecisionBackend, GatherDecisionConfig};
use aft::context::AppContext;
use aft::parser::TreeSitterProvider;
use aft::protocol::{RawRequest, Response};
use aft::search_index::SearchIndex;
use serde_json::json;
use std::sync::Arc;

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
            let mut value = json!({"id":"gather-search", "command":"tool_call", "name":"search", "arguments":args});
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
            if let Some(trace) = trace {
                args["search_context"] = json!(trace);
            }
            handle_semantic_search(&serde_json::from_value(args).unwrap(), &self.ctx)
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
    assert!(gathered.data["text"]
        .as_str()
        .unwrap()
        .starts_with(plain.data["text"].as_str().unwrap()));
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
fn gather_search_test_all_non_ready_states_skip_decision_endpoint() {
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
