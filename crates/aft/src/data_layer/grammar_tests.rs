//! Process-isolated index-store acceptance for the downloaded Luau grammar.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aft_store::ids::{ContentKey, Digest128, FamilyId};
use serde_json::{json, Value};

#[cfg(feature = "wasm-grammars")]
use super::checkout::CheckoutView;
use super::checkout::{Checkout, DataLayer};
use crate::context::AppContext;
use crate::protocol::{RawRequest, Response};

#[cfg(all(
    feature = "wasm-grammars",
    feature = "test-hooks",
    not(all(target_os = "windows", target_arch = "aarch64"))
))]
mod audit;

fn write(root: &Path, path: &str, bytes: impl AsRef<[u8]>) -> PathBuf {
    let path = root.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, bytes).unwrap();
    path
}

fn bind(root: &Path, store: &Path) -> Arc<Checkout> {
    Arc::new(
        Checkout::bind(
            Arc::new(DataLayer::new(8 << 20, 1)),
            store,
            FamilyId::from_digest(Digest128::of(b"luau-s4")),
            root,
        )
        .unwrap(),
    )
}

fn ask(ctx: &AppContext, command: &str, params: Value) -> Response {
    let req = RawRequest {
        id: "luau-s4".into(),
        command: command.into(),
        params,
        lsp_hints: None,
        session_id: None,
    };
    match command {
        "outline" => crate::commands::outline::handle_outline(&req, ctx),
        "zoom" => crate::commands::zoom::handle_zoom(&req, ctx),
        "callers" => crate::commands::callers::handle_callers(&req, ctx),
        "impact" => crate::commands::impact::handle_impact(&req, ctx),
        "trace_to_symbol" => crate::commands::trace_to_symbol::handle_trace_to_symbol(&req, ctx),
        "trace_data" => crate::commands::trace_data::handle_trace_data(&req, ctx),
        "call_tree" => crate::commands::call_tree::handle_call_tree(&req, ctx),
        "trace_to" => crate::commands::trace_to::handle_trace_to(&req, ctx),
        "semantic_search" => crate::commands::semantic_search::handle_semantic_search(&req, ctx),
        "inspect" => crate::commands::inspect::handle_inspect(&req, ctx),
        _ => panic!("unhandled test command"),
    }
}

fn context(root: &Path, storage: &Path) -> AppContext {
    let mut config = crate::config::Config {
        project_root: Some(root.into()),
        storage_dir: Some(storage.into()),
        cache_dir_opt_out: true,
        ..Default::default()
    };
    config.indexes.semantic = false;
    let ctx = AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config);
    ctx.set_canonical_cache_root(root.into());
    ctx
}

fn meaning_query(ctx: &AppContext) -> Response {
    const QUERY: &str = "searchable source input";
    crate::commands::semantic_search::with_precomputed_query_vector(QUERY, vec![1.0, 0.0], || {
        ask(ctx, "semantic_search", json!({"query":QUERY,"top_k":10}))
    })
}

fn enable_meaning(ctx: &AppContext, root: &Path) -> impl Drop {
    ctx.update_config(|config| config.indexes.semantic = true);
    let fill = super::meaning::fill_with_constant_vectors_for_test(root, 2);
    crate::semantic_admission::note_opened("luau-s4", root);
    fill
}

fn wait_meaning(ctx: &AppContext) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if crate::data_layer::tools::search::DataSemantic::current(ctx)
            .and_then(|data| data.serving().cloned())
            .is_some_and(|meaning| meaning.coverage().pending.is_empty())
        {
            return;
        }
        assert!(Instant::now() < deadline, "meaning plane did not settle");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn luau_s4_round4_non_luau_failures_preserve_main_search_contract() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap().join("project");
    std::fs::create_dir_all(&root).unwrap();
    write(
        &root,
        "a.ts",
        "export function searchable() { return 1; }\n",
    );
    write(&root, "b.rs", b"pub fn broken() {\n //\xff\n}\n");
    let ctx = context(&root, &dir.path().join("storage"));
    let _fill = enable_meaning(&ctx, &root);
    ctx.data_checkout()
        .unwrap()
        .wait_for_memory_profile_idle_for_test();
    wait_meaning(&ctx);
    let response = meaning_query(&ctx);
    assert!(response.success, "{:?}", response.data);
    assert_eq!(response.data["complete"], true, "{:?}", response.data);
    assert!(
        response.data.get("semantic_gap").is_none(),
        "{:?}",
        response.data
    );
    let text = response.data["text"].as_str().unwrap();
    assert!(!text.contains("b.rs"), "{text}");
    assert!(!text.contains("utf-8"), "{text}");
    assert!(text.contains("searchable"));
}

#[cfg(feature = "wasm-grammars")]
fn callers(view: &CheckoutView, file: &str) -> Value {
    serde_json::to_value(
        crate::commands::callgraph_store_adapter::callers_result(
            &view.callgraph(),
            &view.root().join(file),
            "target",
            1,
            true,
        )
        .unwrap(),
    )
    .unwrap()
}

#[test]
fn luau_s4_keyed_language_preserves_every_builtin_key() {
    let content = ContentKey::of(b"unchanged");
    const ORIGINAL_EXTRACTOR: &str = "ruled-callgraph-v14-resolver-precision+record-13";
    assert_eq!(super::content::EXTRACTOR_VERSION, ORIGINAL_EXTRACTOR);
    for language in [
        "typescript",
        "tsx",
        "javascript",
        "python",
        "rust",
        "go",
        "c",
        "cpp",
        "cuda",
        "metal",
        "zig",
        "csharp",
        "bash",
        "html",
        "markdown",
        "solidity",
        "scss",
        "vue",
        "json",
        "scala",
        "java",
        "ruby",
        "kotlin",
        "swift",
        "php",
        "lua",
        "perl",
        "yaml",
        "pascal",
        "r",
        "groovy",
        "objc",
        "toml",
        "markers-lexical",
        "markers-markdown",
        "config",
    ] {
        assert_eq!(
            super::content::record_key(content, language),
            aft_store::content::RecordKey::new(content, language, ORIGINAL_EXTRACTOR)
        );
    }
    assert_ne!(
        super::content::record_key(content, "luau"),
        aft_store::content::RecordKey::new(content, "luau", super::content::EXTRACTOR_VERSION)
    );
    assert_eq!(
        crate::parser::detect_language(Path::new("a.lua")),
        Some(crate::parser::LangId::Lua)
    );
    assert_eq!(crate::parser::detect_language(Path::new("a.luau")), None);
}

#[cfg(not(feature = "wasm-grammars"))]
#[test]
fn luau_s4_feature_off_does_not_discover_or_request_a_grammar() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    write(&root, "a.luau", "local function target() end");
    write(
        &root,
        "plain.ts",
        "export function searchable() { return 1; }\n",
    );
    let root = std::fs::canonicalize(root).unwrap();
    let checkout = bind(&root, &dir.path().join("store"));
    checkout.build().unwrap();
    let view = checkout.view().unwrap();
    assert!(view.current_state().skipped_parses().is_empty());
    assert_ne!(
        view.current_state().get(b"a.luau").unwrap().language,
        "luau"
    );
    assert!(!dir.path().join("grammars").exists());
    let ctx = context(&root, &dir.path().join("storage"));
    let outline = ask(&ctx, "outline", json!({"file": root.join("a.luau")}));
    assert_eq!(outline.data["code"], "grammar_unavailable");
    assert_eq!(outline.data["state"], "support_disabled");
    assert_eq!(outline.data["message"], "This AFT build has no downloaded-grammar support, so Luau files can't be parsed. Use a released AFT build, or rebuild with cargo install agent-file-tools --features wasm-grammars (needs CMake 3.18 or later).");
    assert_eq!(outline.data["complete"], false);
    assert_eq!(outline.data["path"], json!(root.join("a.luau")));
    let zoom = ask(
        &ctx,
        "zoom",
        json!({"file": root.join("a.luau"), "symbol": "target"}),
    );
    assert_eq!(zoom.data["code"], "grammar_unavailable");
    assert_eq!(zoom.data["state"], "support_disabled");
    assert_eq!(zoom.data["message"], outline.data["message"]);
    assert_eq!(zoom.data["complete"], false);
    assert_eq!(zoom.data["path"], json!(root.join("a.luau")));
    for command in [
        "callers",
        "impact",
        "call_tree",
        "trace_to",
        "trace_to_symbol",
        "trace_data",
    ] {
        let response = ask(
            &ctx,
            command,
            json!({"file":root.join("a.luau"),"symbol":"target","toSymbol":"target","expression":"input"}),
        );
        assert!(!response.success, "{command}: {:?}", response.data);
        assert_eq!(
            response.data["state"], "support_disabled",
            "{command}: {:?}",
            response.data
        );
        assert_eq!(response.data["complete"], false);
        assert_eq!(response.data["path"], json!(root.join("a.luau")));
    }
    let files = ask(&ctx, "outline", json!({"file": root, "files": true}));
    assert!(files.success, "{:?}", files.data);
    assert_eq!(files.data["complete"], true);
    assert!(files.data.get("gaps").is_none());
    assert!(files.data["text"].as_str().unwrap().contains("a.luau"));
    let directory = ask(&ctx, "outline", json!({"directory":root}));
    assert!(directory.success, "{:?}", directory.data);
    assert_eq!(directory.data["complete"], true, "{:?}", directory.data);
    assert!(!directory.data.to_string().contains("support_disabled"));
    let _fill = enable_meaning(&ctx, &root);
    ctx.data_checkout()
        .unwrap()
        .wait_for_memory_profile_idle_for_test();
    wait_meaning(&ctx);
    let response = meaning_query(&ctx);
    assert!(response.success, "{:?}", response.data);
    assert_eq!(response.data["complete"], true, "{:?}", response.data);
    assert!(response.data.get("semantic_gap").is_none());
    assert!(!response.data.to_string().contains("support_disabled"));
    assert!(response.data["text"]
        .as_str()
        .unwrap()
        .contains("searchable"));
    let builtin_graph = ask(
        &ctx,
        "callers",
        json!({"file":root.join("plain.ts"),"symbol":"searchable"}),
    );
    assert!(builtin_graph.success, "{:?}", builtin_graph.data);
    assert!(builtin_graph.data.get("gaps").is_none());
    assert!(!builtin_graph.data.to_string().contains("support_disabled"));
}

#[cfg(all(
    feature = "wasm-grammars",
    feature = "test-hooks",
    not(all(target_os = "windows", target_arch = "aarch64"))
))]
#[test]
fn luau_s4_feature_matrix() {
    if let Ok(phase) = std::env::var("AFT_S4_PHASE") {
        scenario(&phase);
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let manifest_dir = crate::test_support::manifest_dir().join("tests/fixtures/grammars");
    for (phase, manifest) in [
        ("seed", "manifest-a.json"),
        ("gap", "manifest-a.json"),
        ("identity", "manifest-b.json"),
        ("timeout", "manifest-a.json"),
    ] {
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "data_layer::grammar_tests::luau_s4_feature_matrix",
                "--nocapture",
            ])
            .env("AFT_S4_PHASE", phase)
            .env(
                "AFT_TEST_GRAMMAR_PARSE_DEADLINE_MS",
                if phase == "timeout" { "100" } else { "30000" },
            )
            .env("AFT_S4_ROOT", dir.path())
            .env("AFT_TEST_GRAMMAR_MANIFEST", manifest_dir.join(manifest))
            .env(
                "AFT_TEST_GRAMMAR_TRANSPORT_FILE",
                dir.path().join("transport"),
            )
            .env("AFT_TEST_GRAMMAR_CLOCK_FILE", dir.path().join("clock"))
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{phase}\n{}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
    }
}

#[cfg(all(
    feature = "wasm-grammars",
    feature = "test-hooks",
    not(all(target_os = "windows", target_arch = "aarch64"))
))]
fn scenario(phase: &str) {
    let dir = PathBuf::from(std::env::var_os("AFT_S4_ROOT").unwrap());
    let root = dir.join("project");
    let storage = dir.join("storage");
    let b = "local T = {}\nfunction T.target() return 1 end\nreturn T\n";
    let ts = "export function builtin() { return 1; }\n";
    write(&dir, "clock", "0");
    let ctx = context(&root, &storage);
    let registry = crate::grammar::configure(&ctx.config());
    let manifest = crate::grammar::manifest();
    let wasm = std::fs::read(
        crate::test_support::manifest_dir()
            .join("tests/fixtures/grammars")
            .join(format!("{}.wasm", manifest.sha256)),
    )
    .unwrap();
    if phase == "timeout" {
        registry.stored().unwrap();
        write(
            &root,
            "slow.luau",
            "local function slow(x) if x then return x + 1 else return 0 end end\n".repeat(500),
        );
        let checkout = bind(&root, &dir.join("timeout-store"));
        let report = checkout.build().unwrap();
        assert_eq!(report.extract_failures, 0);
        let view = checkout.view().unwrap();
        assert_eq!(view.current_state().skipped_parses().len(), 1);
        let reason: Value =
            serde_json::from_str(&view.current_state().skipped_parses()[b"slow.luau".as_slice()])
                .unwrap();
        assert_eq!(reason["code"], "grammar_parse_timeout");
        assert_eq!(reason["state"], "parse_timeout");
        assert!(view.callgraph_has_file("b.luau"));
        assert!(!view.callgraph_has_file("slow.luau"));
        assert!(
            !checkout.retry_grammar_gaps().unwrap(),
            "a timeout is per-file, not grammar readiness work"
        );
        assert_eq!(registry.request_count(), 0);
        return;
    }
    if phase == "seed" {
        write(&root, "plain.ts", ts);
        let plain = bind(&root, &dir.join("plain-store"));
        plain.build().unwrap();
        assert_eq!(
            registry.request_count(),
            0,
            "a root without Luau makes no grammar request"
        );
        write(&root, "b.luau", b);
        crate::grammar::test_seams::publish_verified_artifact(
            &crate::root_cache::cache_root(&ctx.config()),
            manifest,
            &wasm,
        )
        .unwrap();
        registry.stored().unwrap();
        let checkout = bind(&root, &dir.join("store"));
        checkout.build().unwrap();
        assert_eq!(registry.request_count(), 0, "stored grammar is offline");
        let view = checkout.view().unwrap();
        assert!(view.current_state().skipped_parses().is_empty());
        std::fs::write(
            dir.join("key-a"),
            super::content::record_key(ContentKey::of(b.as_bytes()), "luau")
                .digest()
                .to_string(),
        )
        .unwrap();
        std::fs::remove_file(
            crate::root_cache::cache_root(&ctx.config())
                .join("grammars")
                .join(format!("{}.wasm", manifest.sha256)),
        )
        .unwrap();
        return;
    }
    if phase == "identity" {
        crate::grammar::test_seams::publish_verified_artifact(
            &crate::root_cache::cache_root(&ctx.config()),
            manifest,
            &wasm,
        )
        .unwrap();
        registry.stored().unwrap();
        let checkout = bind(&root, &dir.join("store"));
        assert!(
            checkout
                .view()
                .is_some_and(|view| !view.callgraph_has_file("b.luau")),
            "old grammar graph cannot serve under a new identity"
        );
        checkout.build().unwrap();
        assert!(super::content::parses_of_for_test(b.as_bytes()) > 0);
        assert_eq!(
            super::content::parses_of_for_test(ts.as_bytes()),
            0,
            "built-in records stay reusable"
        );
        assert_ne!(
            std::fs::read_to_string(dir.join("key-a")).unwrap(),
            super::content::record_key(ContentKey::of(b.as_bytes()), "luau")
                .digest()
                .to_string()
        );
        return;
    }
    write(
        &root,
        "a.luau",
        "local M = require('./b')\nlocal function caller() M.target() end\n",
    );
    let checkout = bind(&root, &dir.join("store"));
    checkout.build().unwrap();
    assert!(registry.wait_until_settled(Duration::from_secs(30)));
    let view = checkout.view().unwrap();
    assert!(
        view.callgraph_has_file("b.luau"),
        "a current stored parse needs no grammar"
    );
    assert!(!view.callgraph_has_file("a.luau"));
    assert_eq!(view.current_state().skipped_parses().len(), 1);
    let response = crate::commands::callgraph_store_adapter::serialized_response_with_view(
        "gap",
        "callers",
        &callers(&view, "b.luau"),
        Some(&view),
        &[&root.join("b.luau")],
    );
    assert_eq!(response.data["complete"], false);
    assert_eq!(
        response.data["gaps"][0]["state"], "network_retry",
        "replies must not retain the scan's downloading state"
    );
    assert_eq!(response.data["gaps"][0]["retry_after_ms"], 30000);
    assert_eq!(response.data["gaps"][0]["path"], json!(root.join("a.luau")));
    assert!(response.data["text"]
        .as_str()
        .unwrap()
        .contains("The Luau grammar"));
    let builtin = json!({"file": "plain.ts", "symbol": "builtin", "total_callers": 0});
    let old = crate::commands::callgraph_store_adapter::serialized_response(
        "builtin", "callers", &builtin,
    );
    let new = crate::commands::callgraph_store_adapter::serialized_response_with_view(
        "builtin",
        "callers",
        &builtin,
        Some(&view),
        &[&root.join("plain.ts")],
    );
    assert_eq!(old.data, new.data, "built-in answers cannot gain Luau gaps");
    assert_eq!(super::content::parses_of_for_test(b.as_bytes()), 0);
    assert_eq!(registry.request_count(), 1);
    let pending = ask(
        &ctx,
        "outline",
        json!({"files": [root.join("plain.ts"), root.join("a.luau")]}),
    );
    assert!(pending.success, "{:?}", pending.data);
    assert_eq!(pending.data["complete"], false);
    assert_eq!(pending.data["skipped_files"], json!([]));
    assert_eq!(pending.data["gaps"][0]["code"], "grammar_unavailable");
    assert!(pending.data["text"].as_str().unwrap().contains("builtin"));
    let pending_zoom = ask(
        &ctx,
        "zoom",
        json!({"targets": [{"file": root.join("plain.ts"), "symbols": "builtin"}, {"file": root.join("a.luau"), "symbols": "caller"}]}),
    );
    assert!(pending_zoom.success, "{:?}", pending_zoom.data);
    assert_eq!(pending_zoom.data["complete"], false);
    assert_eq!(pending_zoom.data["targets"].as_array().unwrap().len(), 2);
    assert_eq!(
        pending_zoom.data["targets"][1]["response"]["complete"],
        false
    );
    assert_eq!(
        pending_zoom.data["targets"][1]["response"]["gaps"][0]["path"],
        json!(root.join("a.luau"))
    );
    assert_eq!(pending_zoom.data["gaps"][0]["code"], "grammar_unavailable");
    let runtime = super::runtime::DataCheckout::open(&root, &storage, None).unwrap();
    let wait = Instant::now();
    while !runtime
        .view()
        .is_some_and(|view| view.current_state().skipped_parses().len() == 2)
    {
        assert!(
            wait.elapsed() < Duration::from_secs(30),
            "bound root did not publish grammar gaps"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let plane = vector_plane(&checkout);
    let coverage = plane.coverage(&view).unwrap();
    assert_eq!(coverage.skipped_parses.len(), 1);
    assert!(coverage.files_failed.is_empty());
    assert!(!coverage.pending.iter().any(|path| path.ends_with("a.luau")));
    write(&dir, "transport", wasm);
    write(&dir, "clock", "30000");
    let started = Instant::now();
    let ready = loop {
        if let Some(view) = runtime.view() {
            if view.current_state().skipped_parses().is_empty() && view.callgraph_has_file("a.luau")
            {
                break view;
            }
        }
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "readiness did not refresh gaps"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(registry.request_count(), 2);
    assert!(callers(&ready, "b.luau").to_string().contains("caller"));
    let cold = bind(&root, &dir.join("cold-store"));
    cold.build().unwrap();
    assert_eq!(
        callers(&ready, "b.luau"),
        callers(&cold.view().unwrap(), "b.luau")
    );
    ready_tools_and_resolver(&ctx, &root, &dir);
}

#[cfg(all(
    feature = "wasm-grammars",
    feature = "test-hooks",
    not(all(target_os = "windows", target_arch = "aarch64"))
))]
fn vector_plane(checkout: &Arc<Checkout>) -> super::vectors::VectorPlane {
    super::vectors::VectorPlane::new(
        Arc::clone(checkout),
        super::vectors::Producer::for_stored_model(aft_store::vector::ModelIdentity {
            name: "luau-fixture".into(),
            revision: "fixture-v1".into(),
            dimensions: 2,
            normalized: false,
            text_template: 1,
            chunk_template: 1,
        }),
        1000,
    )
}

#[cfg(all(
    feature = "wasm-grammars",
    feature = "test-hooks",
    not(all(target_os = "windows", target_arch = "aarch64"))
))]
#[test]
fn luau_s4_callgraph_off_skips_luau_without_blocking_meaning_search() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    write(
        &root,
        "plain.ts",
        "export function searchable() {\n return 1;\n}\n",
    );
    write(
        &root,
        "source.luau",
        "local function luau_function()\n return 1\nend\n",
    );
    let root = std::fs::canonicalize(root).unwrap();
    let ctx = context(&root, &dir.path().join("storage"));
    ctx.update_config(|config| config.indexes.callgraph = false);
    let _fill = enable_meaning(&ctx, &root);
    ctx.data_checkout()
        .unwrap()
        .wait_for_memory_profile_idle_for_test();
    wait_meaning(&ctx);
    let response = meaning_query(&ctx);
    assert!(response.success, "{:?}", response.data);
    assert_eq!(response.data["complete"], false, "{:?}", response.data);
    assert_eq!(response.data["semantic_status"], "ready");
    assert_eq!(response.data["semantic_gap"]["pending"], json!([]));
    assert_eq!(response.data["semantic_gap"]["failed"], json!([]));
    assert_eq!(response.data["semantic_gap"]["skipped_files_count"], 1);
    let reason = &response.data["semantic_gap"]["skipped_files"][0];
    assert_eq!(reason["path"], "source.luau");
    assert_eq!(reason["code"], "callgraph_off");
    assert_eq!(reason["message"], "Luau files are skipped by meaning search because the call-graph index is off (indexes.callgraph: false).");
    let text = response.data["text"].as_str().unwrap();
    assert!(text.contains(reason["message"].as_str().unwrap()));
    assert!(text.contains("source.luau"));
    assert!(text.contains("searchable"));
}

#[cfg(all(
    feature = "wasm-grammars",
    feature = "test-hooks",
    not(all(target_os = "windows", target_arch = "aarch64"))
))]
fn ready_tools_and_resolver(ctx: &AppContext, root: &Path, dir: &Path) {
    let outline = ask(ctx, "outline", json!({"file": root.join("b.luau")}));
    assert!(outline.success, "{:?}", outline.data);
    assert!(outline.data["text"].as_str().unwrap().contains("target"));
    let zoom = ask(
        ctx,
        "zoom",
        json!({"file": root.join("b.luau"), "symbol": "target", "context_lines": 0}),
    );
    assert!(zoom.success, "{:?}", zoom.data);
    assert_eq!(zoom.data["content"], "function T.target() return 1 end");
    let batch = ask(
        ctx,
        "zoom",
        json!({"targets": [{"file": root.join("b.luau"), "symbols": "target"}, {"file": root.join("plain.ts"), "symbols": "builtin"}]}),
    );
    assert!(batch.success, "{:?}", batch.data);
    assert_eq!(batch.data["targets"].as_array().unwrap().len(), 2);

    let matrix = dir.join("matrix");
    let module = "local T = {}\nfunction T.target()\n    return 1\nend\nreturn T\n";
    write(&matrix, "b.luau", module);
    write(&matrix, "other/b.luau", module);
    write(&matrix, "a/x.luau", module);
    write(&matrix, ".luaurc", r#"{"aliases":{"Root":"."}}"#);
    write(
        &matrix,
        "forms.luau",
        include_str!("../../tests/fixtures/grammars/forms.luau"),
    );
    write(&matrix, "a/init.luau", "local A = require('./b')\nlocal B = require('@self/x')\nlocal C = require(script.Parent.b)\nlocal function init_relative() A.target() end\nlocal function init_self() B.target() end\nlocal function init_script() C.target() end\n");
    write(&matrix, "ambig.luau", module);
    write(&matrix, "ambig/init.luau", module);
    write(&matrix, "server.server.luau", module);
    write(
        &matrix,
        "native.lua",
        "local T = {}; function T.target() end; return T",
    );
    write(
        &matrix,
        "symbols.luau",
        include_str!("../../tests/fixtures/luau/symbols.luau"),
    );
    let checkout = bind(&matrix, &dir.join("matrix-store"));
    checkout.build().unwrap();
    let view = checkout.view().unwrap();
    assert!(view.current_state().skipped_parses().is_empty());
    let rows = callers(&view, "b.luau");
    let encoded = rows.to_string();
    for name in [
        "string_call",
        "alias_call",
        "dot_call",
        "bracket_call",
        "wait_call",
        "find_call",
        "init_relative",
        "init_script",
    ] {
        assert!(encoded.contains(name), "missing {name}: {rows}");
    }
    assert_eq!(
        rows["total_callers"], 8,
        "unresolved forms must not guess an edge: {rows}"
    );
    assert!(callers(&view, "a/x.luau").to_string().contains("init_self"));
    for file in [
        "ambig.luau",
        "ambig/init.luau",
        "server.server.luau",
        "native.lua",
    ] {
        assert_eq!(callers(&view, file)["total_callers"], 0, "{file}");
    }
    let matrix_ctx = context(&matrix, &dir.join("matrix-tool-storage"));
    let symbols = ask(
        &matrix_ctx,
        "outline",
        json!({"file": matrix.join("symbols.luau")}),
    );
    assert!(symbols.success, "{:?}", symbols.data);
    for name in ["global", "helper", "Private", "Public", "complex"] {
        assert!(
            symbols.data["text"].as_str().unwrap().contains(name),
            "{:?}",
            symbols.data
        );
    }
    let plane = vector_plane(&checkout);
    let parsed = super::content::parses_of_for_test(module.as_bytes());
    let report = plane
        .fill(
            &mut |texts| Ok(texts.iter().map(|_| vec![1.0, 0.0]).collect()),
            super::vectors::FillBudget::default(),
            &mut |_| {},
        )
        .unwrap();
    assert!(report.files_queued > 0);
    assert_eq!(
        super::content::parses_of_for_test(module.as_bytes()),
        parsed,
        "Luau semantic chunks must use stored records"
    );
    let result = plane.search(&view, &[1.0, 0.0], 100, &|_| true).unwrap();
    assert!(result.coverage.pending.is_empty(), "{:?}", result.coverage);
    assert!(result.coverage.files_failed.is_empty());
    assert!(
        result
            .results
            .iter()
            .any(|row| row.file.ends_with("b.luau") && row.name == "target"),
        "{:?}",
        result.results
    );

    write(&matrix, ".luaurc", r#"{"aliases":{"Root":"other"}}"#);
    checkout.record_paths(&[b".luaurc".to_vec()]).unwrap();
    let update = checkout.refresh_callgraph().unwrap().unwrap();
    assert_eq!(
        update.touched,
        std::collections::BTreeSet::from(["forms.luau".to_string()])
    );
    assert_eq!(update.resolved, 1, "only the alias reader re-resolves");
    let after = checkout.view().unwrap();
    assert!(!callers(&after, "b.luau").to_string().contains("alias_call"));
    assert!(callers(&after, "other/b.luau")
        .to_string()
        .contains("alias_call"));
    checkout.publish_changes().unwrap();
    let cold = bind(&matrix, &dir.join("matrix-cold"));
    cold.build().unwrap();
    assert_eq!(
        callers(&after, "b.luau"),
        callers(&cold.view().unwrap(), "b.luau")
    );
    std::fs::remove_file(matrix.join(".luaurc")).unwrap();
    checkout.record_paths(&[b".luaurc".to_vec()]).unwrap();
    checkout.refresh_callgraph().unwrap();
    assert_eq!(
        callers(&checkout.view().unwrap(), "other/b.luau")["total_callers"],
        0
    );
    write(&matrix, ".luaurc", "not json");
    checkout.record_paths(&[b".luaurc".to_vec()]).unwrap();
    checkout.refresh_callgraph().unwrap();
    assert_eq!(
        callers(&checkout.view().unwrap(), "other/b.luau")["total_callers"],
        0
    );
    write(&matrix, "b/init.luau", module);
    checkout.record_paths(&[b"b/init.luau".to_vec()]).unwrap();
    checkout.refresh_callgraph().unwrap();
    assert_eq!(
        callers(&checkout.view().unwrap(), "b.luau")["total_callers"],
        0
    );
    std::fs::remove_file(matrix.join("b/init.luau")).unwrap();
    checkout.record_paths(&[b"b/init.luau".to_vec()]).unwrap();
    checkout.refresh_callgraph().unwrap();
    assert_eq!(
        callers(&checkout.view().unwrap(), "b.luau")["total_callers"],
        7
    );
}
