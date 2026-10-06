use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use aft::callgraph_store::{project_dead_code_snapshot, CallGraphStore};
use aft::config::Config;
use aft::inspect::oxc_engine::{analyze_files, AnalyzeOptions, LivenessVerdict, OxcEngineResult};
use aft::inspect::scanners::dead_code::run_dead_code_scan;
use aft::inspect::{
    CallgraphExport, CallgraphOutboundCall, CallgraphSnapshot, InspectCategory, InspectJob,
    InspectScanSuccess, JobKey,
};
use aft::parser::SymbolCache;
use serde_json::json;

fn fixture_project(files: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf, Vec<PathBuf>) {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let root = temp_dir.path().join("project");
    fs::create_dir_all(&root).expect("create project root");

    let paths = files
        .iter()
        .map(|(relative, contents)| {
            let path = root.join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("create parent");
            }
            fs::write(&path, contents).expect("write fixture file");
            path
        })
        .collect::<Vec<_>>();

    (temp_dir, root, paths)
}

fn job(
    root: &Path,
    scope_files: Vec<PathBuf>,
    callgraph_snapshot: Option<CallgraphSnapshot>,
) -> InspectJob {
    InspectJob {
        job_id: 1,
        key: JobKey::for_project_category(InspectCategory::DeadCode),
        category: InspectCategory::DeadCode,
        scope_files,
        project_root: root.to_path_buf(),
        inspect_dir: root.join(".aft-cache").join("inspect"),
        config: Arc::new(Config {
            project_root: Some(root.to_path_buf()),
            ..Config::default()
        }),
        symbol_cache: Arc::new(RwLock::new(SymbolCache::new())),
        inspect_writer: true,
        callgraph_writer: true,
        callgraph_snapshot: callgraph_snapshot.map(Arc::new),
    }
}

fn snapshot(
    files: Vec<PathBuf>,
    exported_symbols: Vec<CallgraphExport>,
    outbound_calls: Vec<CallgraphOutboundCall>,
    entry_points: Vec<PathBuf>,
) -> CallgraphSnapshot {
    CallgraphSnapshot {
        generated_at: None,
        files,
        exported_symbols,
        outbound_calls,
        entry_points: entry_points.into_iter().collect::<BTreeSet<_>>(),
        entry_point_symbols: BTreeMap::new(),
    }
}

fn export(root: &Path, file: &str, symbol: &str, kind: &str, line: u32) -> CallgraphExport {
    CallgraphExport {
        file: root.join(file),
        symbol: symbol.to_string(),
        kind: kind.to_string(),
        line,
    }
}

fn outbound(
    root: &Path,
    caller_file: &str,
    caller_symbol: &str,
    target: &str,
    line: u32,
) -> CallgraphOutboundCall {
    CallgraphOutboundCall {
        caller_file: root.join(caller_file),
        caller_symbol: caller_symbol.to_string(),
        target: target.to_string(),
        line,
        provenance: "treesitter".to_string(),
    }
}

fn target(root: &Path, file: &str, symbol: &str) -> String {
    // Join per component so "src/handlers.rs" becomes a native path on every
    // platform; a raw join would keep the forward slash inside the Windows path
    // and never match the scanner's all-native-separator output.
    let mut path = root.to_path_buf();
    for component in file.split('/') {
        path.push(component);
    }

    // Snapshot paths remove Windows verbatim prefixes during normalization.
    // Normalize every absolute target expectation the same way, including roots
    // returned by fs::canonicalize as `\\?\C:\...`.
    #[cfg(windows)]
    let path = {
        let display = path.to_string_lossy();
        if let Some(stripped) = display.strip_prefix(r"\\?\UNC\") {
            PathBuf::from(format!(r"\\{stripped}"))
        } else if let Some(stripped) = display.strip_prefix(r"\\?\") {
            PathBuf::from(stripped)
        } else {
            path
        }
    };

    format!("{}::{symbol}", path.display())
}

fn scan(job: InspectJob) -> InspectScanSuccess {
    run_dead_code_scan(&job).outcome.expect("scan succeeds")
}

fn projected_snapshot_from_store(
    root: &Path,
    files: &[PathBuf],
    store_dir: &str,
) -> CallgraphSnapshot {
    let store = CallGraphStore::open(root.join(store_dir), root.to_path_buf()).expect("open store");
    store.cold_build(files).expect("cold build store");
    project_dead_code_snapshot(store.sqlite_path()).expect("project dead-code snapshot")
}

fn outbound_call_set_bytes(snapshot: &CallgraphSnapshot) -> String {
    let mut rows = snapshot
        .outbound_calls
        .iter()
        .map(|call| {
            format!(
                "{}\t{}\t{}\t{}\t{}",
                call.caller_file.display(),
                call.caller_symbol,
                call.target,
                call.line,
                call.provenance
            )
        })
        .collect::<Vec<_>>();
    rows.sort();
    serde_json::to_string(&rows).expect("serialize outbound rows")
}

fn aggregate_has_item(success: &InspectScanSuccess, file: &str, symbol: &str) -> bool {
    success.aggregate["items"]
        .as_array()
        .expect("dead-code items")
        .iter()
        .any(|item| item["file"] == file && item["symbol"] == symbol)
}

fn aggregate_test_only_item<'a>(
    success: &'a InspectScanSuccess,
    file: &str,
    symbol: &str,
) -> Option<&'a serde_json::Value> {
    success.aggregate["test_only_items"]
        .as_array()
        .expect("test-only dead-code items")
        .iter()
        .find(|item| item["file"] == file && item["symbol"] == symbol)
}

fn aggregate_has_item_kind(
    success: &InspectScanSuccess,
    file: &str,
    symbol: &str,
    kind: &str,
) -> bool {
    success.aggregate["items"]
        .as_array()
        .expect("dead-code items")
        .iter()
        .any(|item| item["file"] == file && item["symbol"] == symbol && item["kind"] == kind)
}

fn assert_oxc_verdict(
    result: &OxcEngineResult,
    file: &str,
    symbol: &str,
    expected: LivenessVerdict,
) {
    let export = result
        .files
        .iter()
        .find(|item| item.relative_file == file)
        .unwrap_or_else(|| panic!("missing file verdicts for {file}: {:#?}", result.files))
        .exports
        .iter()
        .find(|export| export.symbol == symbol)
        .unwrap_or_else(|| panic!("missing export {file}:{symbol}: {:#?}", result.files));
    assert_eq!(
        export.verdict, expected,
        "unexpected verdict for {file}:{symbol}: {export:#?}"
    );
}

fn dispatched_target(target: &str, full_callee: &str) -> String {
    format!("{target}\u{1f}{full_callee}")
}

fn contribution_bytes(success: &InspectScanSuccess) -> String {
    let mut rows = success
        .contributions
        .iter()
        .map(|contribution| {
            (
                contribution.contribution["file"]
                    .as_str()
                    .expect("contribution file")
                    .to_string(),
                contribution.contribution.clone(),
            )
        })
        .collect::<Vec<_>>();
    rows.sort_by(|left, right| left.0.cmp(&right.0));
    serde_json::to_string(&rows).expect("serialize contribution rows")
}

fn type_match_fixture_exports(root: &Path) -> Vec<CallgraphExport> {
    vec![
        export(root, "src/factory.rs", "make_live", "function", 3),
        export(root, "src/live_widget.rs", "new", "method", 4),
        export(
            root,
            "src/planted_dead.rs",
            "orphan_function",
            "function",
            1,
        ),
        export(root, "src/planted_dead.rs", "new", "method", 6),
    ]
}

#[test]
fn inspect_dead_code_unavailable_callgraph_returns_empty_result() {
    let (_temp_dir, root, paths) = fixture_project(&[("src/foo.ts", "export function foo() {}\n")]);

    let success = scan(job(&root, paths, None));

    assert!(success.contributions.is_empty());
    assert!(
        success.aggregate.get("count").is_none(),
        "unavailable callgraph must not be represented as zero dead code"
    );
    assert_eq!(success.aggregate["by_language"], json!({}));
    assert_eq!(success.aggregate["callgraph_available"], false);
    assert_eq!(success.aggregate["drill_down_capped"], false);
}

#[test]
fn inspect_dead_code_reports_exported_uncalled_function() {
    let (_temp_dir, root, paths) =
        fixture_project(&[("src/foo.ts", "export function unused() {}\n")]);
    let graph = snapshot(
        paths.clone(),
        vec![export(&root, "src/foo.ts", "unused", "function", 1)],
        Vec::new(),
        Vec::new(),
    );

    let success = scan(job(&root, paths, Some(graph)));

    assert_eq!(success.aggregate["count"], 1);
    assert_eq!(success.aggregate["by_language"]["typescript"], 1);
    assert_eq!(
        success.aggregate["items"].as_array().expect("items").len(),
        1
    );
    assert_eq!(
        success.aggregate["items"][0],
        json!({"file": "src/foo.ts", "symbol": "unused", "kind": "function", "line": 1})
    );
}

#[test]
fn inspect_dead_code_skips_kotlin_without_dead_verdicts() {
    let (_temp_dir, root, paths) = fixture_project(&[(
        "src/Sample.kt",
        "class UnreferencedService { fun run() = Unit }\n",
    )]);
    let graph = snapshot(
        paths.clone(),
        vec![export(
            &root,
            "src/Sample.kt",
            "UnreferencedService",
            "class",
            1,
        )],
        Vec::new(),
        Vec::new(),
    );

    let success = scan(job(&root, paths, Some(graph)));

    assert_eq!(success.aggregate["count"], 0);
    assert_eq!(success.aggregate["by_language"], json!({}));
    assert!(success.aggregate["items"].as_array().unwrap().is_empty());
    assert_eq!(success.aggregate["languages_skipped"], json!(["kotlin"]));
    assert!(success.contributions[0].contribution["exports"]
        .as_array()
        .unwrap()
        .is_empty());
}

#[test]
fn inspect_dead_code_does_not_report_export_reachable_from_entry_point() {
    let (_temp_dir, root, paths) = fixture_project(&[
        ("src/foo.ts", "export function used() {}\n"),
        ("src/main.ts", "export function main() {\n  used();\n}\n"),
    ]);
    let graph = snapshot(
        paths.clone(),
        vec![
            export(&root, "src/foo.ts", "used", "function", 1),
            export(&root, "src/main.ts", "main", "function", 1),
        ],
        vec![outbound(
            &root,
            "src/main.ts",
            "main",
            &target(&root, "src/foo.ts", "used"),
            2,
        )],
        vec![root.join("src/main.ts")],
    );

    let success = scan(job(&root, paths, Some(graph)));

    assert_eq!(success.aggregate["count"], 0);
    assert!(success.aggregate["items"]
        .as_array()
        .expect("items")
        .is_empty());
}

#[test]
fn inspect_dead_code_keeps_multi_hop_entry_point_reachability_alive() {
    let (_temp_dir, root, paths) = fixture_project(&[
        ("src/entry.ts", "export function entry() {\n  b();\n}\n"),
        ("src/b.ts", "export function b() {\n  c();\n}\n"),
        ("src/c.ts", "export function c() {}\n"),
    ]);
    let graph = snapshot(
        paths.clone(),
        vec![
            export(&root, "src/entry.ts", "entry", "function", 1),
            export(&root, "src/b.ts", "b", "function", 1),
            export(&root, "src/c.ts", "c", "function", 1),
        ],
        vec![
            outbound(
                &root,
                "src/entry.ts",
                "entry",
                &target(&root, "src/b.ts", "b"),
                2,
            ),
            outbound(&root, "src/b.ts", "b", &target(&root, "src/c.ts", "c"), 2),
        ],
        vec![root.join("src/entry.ts")],
    );

    let success = scan(job(&root, paths, Some(graph)));

    assert_eq!(success.aggregate["count"], 0);
}

#[test]
fn inspect_dead_code_keeps_same_name_exports_distinct() {
    let (_temp_dir, root, paths) = fixture_project(&[
        ("src/entry.ts", "export function main() {\n  foo();\n}\n"),
        ("src/dead.ts", "export function foo() {}\n"),
        ("src/alive.ts", "export function foo() {}\n"),
    ]);
    let graph = snapshot(
        paths.clone(),
        vec![
            export(&root, "src/entry.ts", "main", "function", 1),
            export(&root, "src/dead.ts", "foo", "function", 1),
            export(&root, "src/alive.ts", "foo", "function", 1),
        ],
        vec![outbound(
            &root,
            "src/entry.ts",
            "main",
            &target(&root, "src/alive.ts", "foo"),
            2,
        )],
        vec![root.join("src/entry.ts")],
    );

    let success = scan(job(&root, paths, Some(graph)));

    assert_eq!(success.aggregate["count"], 1);
    assert_eq!(
        success.aggregate["items"][0],
        json!({"file": "src/dead.ts", "symbol": "foo", "kind": "function", "line": 1})
    );
}

#[test]
fn inspect_dead_code_attributes_calls_to_recorded_caller_symbol() {
    let (_temp_dir, root, paths) = fixture_project(&[
        (
            "src/a.ts",
            "export function a() {\n  return 1;\n}\nexport function b() {}\n",
        ),
        ("src/target.ts", "export function target() {}\n"),
    ]);
    let graph = snapshot(
        paths.clone(),
        vec![
            export(&root, "src/a.ts", "a", "function", 1),
            export(&root, "src/a.ts", "b", "function", 4),
            export(&root, "src/target.ts", "target", "function", 1),
        ],
        vec![outbound(
            &root,
            "src/a.ts",
            "a",
            &target(&root, "src/target.ts", "target"),
            6,
        )],
        vec![root.join("src/a.ts")],
    );

    let success = scan(job(&root, paths, Some(graph)));

    assert_eq!(success.aggregate["count"], 1);
    assert_eq!(
        success.aggregate["items"][0],
        json!({"file": "src/a.ts", "symbol": "b", "kind": "function", "line": 4})
    );
}

#[test]
fn inspect_dead_code_flags_binary_export_but_suppresses_library_public_api() {
    let (_temp_dir, root, paths) = fixture_project(&[
        (
            "Cargo.toml",
            "[package]\nname = \"inspect-fixture\"\nversion = \"0.1.0\"\n",
        ),
        ("src/main.rs", "pub fn unused_internal() {}\nfn main() {}\n"),
        ("src/lib.rs", "pub fn public_api() {}\n"),
    ]);
    let source_files = vec![root.join("src/main.rs"), root.join("src/lib.rs")];
    let graph = snapshot(
        source_files,
        vec![
            export(&root, "src/main.rs", "unused_internal", "function", 1),
            export(&root, "src/lib.rs", "public_api", "function", 1),
        ],
        Vec::new(),
        vec![root.join("src/main.rs")],
    );

    let success = scan(job(&root, paths, Some(graph)));

    assert_eq!(success.aggregate["count"], 1);
    assert_eq!(
        success.aggregate["items"][0],
        json!({"file": "src/main.rs", "symbol": "unused_internal", "kind": "function", "line": 1})
    );
}

#[test]
fn inspect_dead_code_keeps_outbound_contributions_identical_when_grouped_by_caller_file() {
    let (_temp_dir, root, paths) = fixture_project(&[
        (
            "src/app.ts",
            "import { helper } from './helper';\nexport function main(service: Service) { helper(); service.render(); }\n",
        ),
        (
            "src/service.ts",
            "export class Service { render() { finish(); } dormant() { orphan(); } }\n",
        ),
        ("src/helper.ts", "export function helper() {}\n"),
        ("src/finish.ts", "export function finish() {}\n"),
        ("src/orphan.ts", "export function orphan() {}\n"),
    ]);
    let normalized_app_caller = root.join("src").join("nested").join("..").join("app.ts");
    let graph = snapshot(
        paths.clone(),
        vec![
            export(&root, "src/app.ts", "main", "function", 2),
            export(&root, "src/service.ts", "render", "method", 1),
            export(&root, "src/service.ts", "dormant", "method", 1),
            export(&root, "src/helper.ts", "helper", "function", 1),
            export(&root, "src/finish.ts", "finish", "function", 1),
            export(&root, "src/orphan.ts", "orphan", "function", 1),
        ],
        vec![
            CallgraphOutboundCall {
                caller_file: normalized_app_caller.clone(),
                caller_symbol: "main".to_string(),
                target: target(&root, "src/helper.ts", "helper"),
                line: 2,
                provenance: "treesitter".to_string(),
            },
            CallgraphOutboundCall {
                caller_file: normalized_app_caller,
                caller_symbol: "main".to_string(),
                target: dispatched_target("render", "service.render"),
                line: 3,
                provenance: "treesitter".to_string(),
            },
            outbound(
                &root,
                "src/service.ts",
                "Service::render",
                &target(&root, "src/finish.ts", "finish"),
                6,
            ),
            outbound(
                &root,
                "src/service.ts",
                "Service::dormant",
                &target(&root, "src/orphan.ts", "orphan"),
                7,
            ),
        ],
        vec![root.join("src/app.ts")],
    );

    let success = scan(job(&root, paths, Some(graph)));

    assert_eq!(success.aggregate["count"], 2);
    assert!(aggregate_has_item(&success, "src/service.ts", "dormant"));
    assert!(aggregate_has_item(&success, "src/orphan.ts", "orphan"));
    assert!(!aggregate_has_item(&success, "src/service.ts", "render"));
    assert!(!aggregate_has_item(&success, "src/finish.ts", "finish"));
    assert!(!aggregate_has_item(&success, "src/helper.ts", "helper"));
}

#[test]
fn inspect_dead_code_reports_unreachable_cycle_exports() {
    let (_temp_dir, root, paths) = fixture_project(&[
        ("src/a.ts", "export function a() {\n  b();\n}\n"),
        ("src/b.ts", "export function b() {\n  a();\n}\n"),
    ]);
    let graph = snapshot(
        paths.clone(),
        vec![
            export(&root, "src/a.ts", "a", "function", 1),
            export(&root, "src/b.ts", "b", "function", 1),
        ],
        vec![
            outbound(&root, "src/a.ts", "a", &target(&root, "src/b.ts", "b"), 2),
            outbound(&root, "src/b.ts", "b", &target(&root, "src/a.ts", "a"), 2),
        ],
        Vec::new(),
    );

    let success = scan(job(&root, paths, Some(graph)));

    let dead_symbols = success.aggregate["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|item| {
            (
                item["file"].as_str().expect("file").to_string(),
                item["symbol"].as_str().expect("symbol").to_string(),
            )
        })
        .collect::<BTreeSet<_>>();

    assert_eq!(success.aggregate["count"], 2);
    assert_eq!(success.aggregate["by_language"]["typescript"], 2);
    assert_eq!(
        dead_symbols,
        BTreeSet::from([
            ("src/a.ts".to_string(), "a".to_string()),
            ("src/b.ts".to_string(), "b".to_string()),
        ])
    );
}

#[test]
fn inspect_dead_code_does_not_report_entry_point_exports() {
    let (_temp_dir, root, paths) =
        fixture_project(&[("src/main.ts", "export function main() {}\n")]);
    let graph = snapshot(
        paths.clone(),
        vec![export(&root, "src/main.ts", "main", "function", 1)],
        Vec::new(),
        vec![root.join("src/main.ts")],
    );

    let success = scan(job(&root, paths, Some(graph)));

    assert_eq!(success.aggregate["count"], 0);
}

#[test]
fn inspect_dead_code_does_not_report_package_json_main_export() {
    let (_temp_dir, root, paths) = fixture_project(&[
        ("package.json", "{\"main\":\"src/public.ts\"}\n"),
        ("src/public.ts", "export function publicApi() {}\n"),
    ]);
    let source_files = vec![root.join("src/public.ts")];
    let graph = snapshot(
        source_files.clone(),
        vec![export(&root, "src/public.ts", "publicApi", "function", 1)],
        Vec::new(),
        Vec::new(),
    );

    let success = scan(job(&root, paths, Some(graph)));

    assert_eq!(success.aggregate["count"], 0);
}

#[test]
fn inspect_dead_code_resolves_extensionless_package_json_main_export() {
    let (_temp_dir, root, paths) = fixture_project(&[
        ("package.json", "{\"main\":\"src/index\"}\n"),
        ("src/index.ts", "export function publicApi() {}\n"),
    ]);
    let source_files = vec![root.join("src/index.ts")];
    let graph = snapshot(
        source_files,
        vec![export(&root, "src/index.ts", "publicApi", "function", 1)],
        Vec::new(),
        Vec::new(),
    );

    let success = scan(job(&root, paths, Some(graph)));

    assert_eq!(success.aggregate["count"], 0);
}

#[test]
fn inspect_dead_code_keeps_cross_package_barrel_reexport_import_live() {
    let (_temp_dir, root, _paths) = fixture_project(&[
        (
            "package.json",
            r#"{"private":true,"workspaces":["packages/*"]}"#,
        ),
        (
            "packages/bridge/package.json",
            r#"{"name":"@scope/bridge","exports":"./src/index.ts"}"#,
        ),
        (
            "packages/bridge/src/index.ts",
            "export type { LiveEnvelope } from \"./protocol.js\";\n",
        ),
        (
            "packages/bridge/src/protocol.ts",
            "export interface LiveEnvelope { id: string; }\nexport interface DeadEnvelope { id: string; }\n",
        ),
        ("packages/app/package.json", r#"{"name":"app"}"#),
        (
            "packages/app/src/consumer.ts",
            "import type { LiveEnvelope as DownstreamEnvelope } from \"@scope/bridge\";\ntype ConsumerEnvelope = DownstreamEnvelope;\n",
        ),
    ]);
    let source_files = vec![
        root.join("packages/bridge/src/index.ts"),
        root.join("packages/bridge/src/protocol.ts"),
        root.join("packages/app/src/consumer.ts"),
    ];
    let graph = snapshot(
        source_files.clone(),
        vec![
            export(
                &root,
                "packages/bridge/src/index.ts",
                "LiveEnvelope",
                "interface",
                1,
            ),
            export(
                &root,
                "packages/bridge/src/protocol.ts",
                "LiveEnvelope",
                "interface",
                1,
            ),
            export(
                &root,
                "packages/bridge/src/protocol.ts",
                "DeadEnvelope",
                "interface",
                2,
            ),
        ],
        Vec::new(),
        vec![root.join("packages/app/src/consumer.ts")],
    );

    let success = scan(job(&root, source_files, Some(graph)));
    let dead_symbols = success.aggregate["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|item| item["symbol"].as_str().expect("symbol").to_string())
        .collect::<BTreeSet<_>>();

    assert_eq!(success.aggregate["count"], 1, "{:#}", success.aggregate);
    assert!(
        !dead_symbols.contains("LiveEnvelope"),
        "barrel-imported cross-package type should be live: {:#}",
        success.aggregate
    );
    assert!(dead_symbols.contains("DeadEnvelope"));
}

#[test]
fn inspect_dead_code_keeps_workspace_barrel_default_export_import_live() {
    let (_temp_dir, root, _paths) = fixture_project(&[
        (
            "package.json",
            r#"{"private":true,"workspaces":["packages/*"]}"#,
        ),
        (
            "packages/bridge/package.json",
            r#"{"name":"@scope/bridge","exports":"./src/index.ts"}"#,
        ),
        (
            "packages/bridge/src/index.ts",
            "export { default } from \"./value\";\n",
        ),
        (
            "packages/bridge/src/value.ts",
            "export default function LiveDefault() { return 1; }\nexport function deadHelper() { return 2; }\n",
        ),
        ("packages/app/package.json", r#"{"name":"app"}"#),
        (
            "packages/app/src/consumer.ts",
            "import LiveDefault from \"@scope/bridge\";\nLiveDefault();\n",
        ),
    ]);
    let source_files = vec![
        root.join("packages/bridge/src/index.ts"),
        root.join("packages/bridge/src/value.ts"),
        root.join("packages/app/src/consumer.ts"),
    ];
    let graph = snapshot(
        source_files.clone(),
        vec![
            export(
                &root,
                "packages/bridge/src/value.ts",
                "LiveDefault",
                "function",
                1,
            ),
            export(
                &root,
                "packages/bridge/src/value.ts",
                "LiveDefault",
                "default_export",
                1,
            ),
            export(
                &root,
                "packages/bridge/src/value.ts",
                "deadHelper",
                "function",
                2,
            ),
        ],
        Vec::new(),
        vec![root.join("packages/app/src/consumer.ts")],
    );

    let success = scan(job(&root, source_files, Some(graph)));
    let dead_symbols = success.aggregate["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|item| item["symbol"].as_str().expect("symbol").to_string())
        .collect::<BTreeSet<_>>();

    assert_eq!(success.aggregate["count"], 1, "{:#}", success.aggregate);
    assert!(
        !dead_symbols.contains("LiveDefault"),
        "barrel-imported default export should be live: {:#}",
        success.aggregate
    );
    assert!(dead_symbols.contains("deadHelper"));
}

#[test]
fn inspect_dead_code_does_not_keep_namespace_imported_exports_live_from_dead_file() {
    let (_temp_dir, root, paths) = fixture_project(&[
        (
            "src/mod.ts",
            "export function thing() { return 1; }\nexport function helper() { return 2; }\n",
        ),
        (
            "src/dead_consumer.ts",
            "import * as mod from \"./mod\";\nmod.thing();\n",
        ),
    ]);
    let graph = snapshot(
        paths.clone(),
        vec![
            export(&root, "src/mod.ts", "thing", "function", 1),
            export(&root, "src/mod.ts", "helper", "function", 2),
        ],
        vec![outbound(
            &root,
            "src/dead_consumer.ts",
            "<top-level>",
            &target(&root, "src/mod.ts", "thing"),
            2,
        )],
        Vec::new(),
    );

    let success = scan(job(&root, paths, Some(graph)));
    let dead_symbols = success.aggregate["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|item| item["symbol"].as_str().expect("symbol").to_string())
        .collect::<BTreeSet<_>>();

    assert_eq!(success.aggregate["count"], 2, "{:#}", success.aggregate);
    assert!(dead_symbols.contains("thing"));
    assert!(dead_symbols.contains("helper"));
}

#[test]
fn inspect_dead_code_keeps_namespace_imported_exports_live_from_reachable_file() {
    let (_temp_dir, root, paths) = fixture_project(&[
        (
            "src/mod.ts",
            "export function thing() { return 1; }\nexport function helper() { return 2; }\n",
        ),
        (
            "src/consumer.ts",
            "import * as mod from \"./mod\";\nmod.thing();\n",
        ),
    ]);
    let graph = snapshot(
        paths.clone(),
        vec![
            export(&root, "src/mod.ts", "thing", "function", 1),
            export(&root, "src/mod.ts", "helper", "function", 2),
        ],
        vec![outbound(
            &root,
            "src/consumer.ts",
            "<top-level>",
            &target(&root, "src/mod.ts", "thing"),
            2,
        )],
        vec![root.join("src/consumer.ts")],
    );

    let success = scan(job(&root, paths, Some(graph)));

    assert_eq!(success.aggregate["count"], 0, "{:#}", success.aggregate);
    assert!(success.aggregate["items"]
        .as_array()
        .expect("items")
        .is_empty());
}

#[test]
fn inspect_dead_code_skips_malformed_child_package_json_when_resolving_package_name() {
    let (_temp_dir, root, _paths) = fixture_project(&[
        (
            "package.json",
            r#"{"private":true,"workspaces":["packages/*"]}"#,
        ),
        (
            "packages/bridge/package.json",
            r#"{"name":"@scope/bridge","exports":"./src/index.ts"}"#,
        ),
        ("packages/bridge/src/package.json", "{ malformed json"),
        (
            "packages/bridge/src/index.ts",
            "export function liveFunction() { return 1; }\n",
        ),
        ("packages/app/package.json", r#"{"name":"app"}"#),
        (
            "packages/app/src/consumer.ts",
            "import { liveFunction } from \"@scope/bridge\";\nliveFunction();\n",
        ),
    ]);
    let source_files = vec![
        root.join("packages/bridge/src/index.ts"),
        root.join("packages/app/src/consumer.ts"),
    ];
    let graph = snapshot(
        source_files.clone(),
        vec![export(
            &root,
            "packages/bridge/src/index.ts",
            "liveFunction",
            "function",
            1,
        )],
        Vec::new(),
        vec![root.join("packages/app/src/consumer.ts")],
    );

    let success = scan(job(&root, source_files, Some(graph)));

    assert_eq!(success.aggregate["count"], 0, "{:#}", success.aggregate);
}

#[test]
fn inspect_dead_code_keeps_relative_named_import_live_from_reachable_file() {
    let (_temp_dir, root, paths) = fixture_project(&[
        (
            "src/main.ts",
            "import { Live } from './leaf';\nexport function main() { return 1; }\n",
        ),
        (
            "src/leaf.ts",
            "export function Live() { return 1; }\nexport function Dead() { return 2; }\n",
        ),
    ]);
    let graph = snapshot(
        paths.clone(),
        vec![
            export(&root, "src/main.ts", "main", "function", 2),
            export(&root, "src/leaf.ts", "Live", "function", 1),
            export(&root, "src/leaf.ts", "Dead", "function", 2),
        ],
        Vec::new(),
        vec![root.join("src/main.ts")],
    );

    let success = scan(job(&root, paths, Some(graph)));
    let dead_symbols = success.aggregate["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|item| item["symbol"].as_str().expect("symbol").to_string())
        .collect::<BTreeSet<_>>();

    assert_eq!(success.aggregate["count"], 1, "{:#}", success.aggregate);
    assert!(!dead_symbols.contains("Live"));
    assert!(dead_symbols.contains("Dead"));
}

#[test]
fn inspect_dead_code_executes_side_effect_imports_without_marking_exports_used() {
    let (_temp_dir, root, paths) = fixture_project(&[
        ("src/main.ts", "import './setup';\n"),
        (
            "src/setup.ts",
            "import { used } from './dep';\nexport function init() { return 1; }\nexport function unusedHelper() { return 2; }\ninit();\nused();\n",
        ),
        ("src/dep.ts", "export function used() { return 1; }\n"),
    ]);

    let result = analyze_files(
        &root,
        &paths,
        AnalyzeOptions {
            entry_points: vec![root.join("src/main.ts")],
            entry_reachability: true,
            ..AnalyzeOptions::default()
        },
    )
    .expect("oxc analyze succeeds");

    assert_oxc_verdict(&result, "src/setup.ts", "init", LivenessVerdict::Used);
    assert_oxc_verdict(
        &result,
        "src/setup.ts",
        "unusedHelper",
        LivenessVerdict::Unused,
    );
    assert_oxc_verdict(&result, "src/dep.ts", "used", LivenessVerdict::Used);
}

#[test]
fn inspect_dead_code_does_not_keep_relative_named_import_live_from_dead_file() {
    let (_temp_dir, root, paths) = fixture_project(&[
        (
            "src/dead_consumer.ts",
            "import { Live } from './leaf';\nexport function unusedConsumer() { return 1; }\n",
        ),
        (
            "src/leaf.ts",
            "export function Live() { return 1; }\nexport function Dead() { return 2; }\n",
        ),
    ]);
    let graph = snapshot(
        paths.clone(),
        vec![
            export(
                &root,
                "src/dead_consumer.ts",
                "unusedConsumer",
                "function",
                2,
            ),
            export(&root, "src/leaf.ts", "Live", "function", 1),
            export(&root, "src/leaf.ts", "Dead", "function", 2),
        ],
        Vec::new(),
        Vec::new(),
    );

    let success = scan(job(&root, paths, Some(graph)));
    let dead_symbols = success.aggregate["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|item| item["symbol"].as_str().expect("symbol").to_string())
        .collect::<BTreeSet<_>>();

    assert_eq!(success.aggregate["count"], 3, "{:#}", success.aggregate);
    assert!(dead_symbols.contains("unusedConsumer"));
    assert!(dead_symbols.contains("Live"));
    assert!(dead_symbols.contains("Dead"));
}

#[test]
fn inspect_dead_code_keeps_public_typescript_barrel_reexport_leaf_live() {
    let (_temp_dir, root, _paths) = fixture_project(&[
        ("package.json", r#"{"main":"src/index.ts"}"#),
        ("src/index.ts", "export { Live } from './leaf';\n"),
        (
            "src/leaf.ts",
            "export function Live() { return 1; }\nexport function Dead() { return 2; }\n",
        ),
    ]);
    let source_files = vec![root.join("src/index.ts"), root.join("src/leaf.ts")];
    let graph = snapshot(
        source_files.clone(),
        vec![
            export(&root, "src/index.ts", "Live", "re_export", 1),
            export(&root, "src/leaf.ts", "Live", "function", 1),
            export(&root, "src/leaf.ts", "Dead", "function", 2),
        ],
        Vec::new(),
        Vec::new(),
    );

    let success = scan(job(&root, source_files, Some(graph)));
    let dead_symbols = success.aggregate["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|item| item["symbol"].as_str().expect("symbol").to_string())
        .collect::<BTreeSet<_>>();

    assert_eq!(success.aggregate["count"], 1, "{:#}", success.aggregate);
    assert!(!dead_symbols.contains("Live"));
    assert!(dead_symbols.contains("Dead"));
}

#[test]
fn inspect_dead_code_keeps_public_rust_pub_use_leaf_live() {
    let (_temp_dir, root, _paths) = fixture_project(&[
        (
            "Cargo.toml",
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n",
        ),
        ("src/lib.rs", "pub use foo::Foo;\nmod foo;\n"),
        ("src/foo.rs", "pub struct Foo;\npub struct Dead;\n"),
    ]);
    let source_files = vec![root.join("src/lib.rs"), root.join("src/foo.rs")];
    let graph = snapshot(
        source_files.clone(),
        vec![
            export(&root, "src/lib.rs", "Foo", "struct", 1),
            export(&root, "src/foo.rs", "Foo", "struct", 1),
            export(&root, "src/foo.rs", "Dead", "struct", 2),
        ],
        Vec::new(),
        Vec::new(),
    );

    let success = scan(job(&root, source_files, Some(graph)));
    let dead_symbols = success.aggregate["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|item| item["symbol"].as_str().expect("symbol").to_string())
        .collect::<BTreeSet<_>>();

    assert_eq!(success.aggregate["count"], 1, "{:#}", success.aggregate);
    assert!(!dead_symbols.contains("Foo"));
    assert!(dead_symbols.contains("Dead"));
}

#[test]
fn inspect_dead_code_parses_rust_scoped_targets_after_file_separator() {
    let (_temp_dir, root, paths) = fixture_project(&[
        ("src/main.rs", "pub fn main() { Handler::handle(); }\n"),
        ("src/handler.rs", "pub fn handle() {}\n"),
    ]);
    let graph = snapshot(
        paths.clone(),
        vec![
            export(&root, "src/main.rs", "main", "function", 1),
            export(&root, "src/handler.rs", "Handler::handle", "method", 1),
        ],
        vec![outbound(
            &root,
            "src/main.rs",
            "main",
            &target(&root, "src/handler.rs", "Handler::handle"),
            1,
        )],
        vec![root.join("src/main.rs")],
    );

    let success = scan(job(&root, paths, Some(graph)));

    assert_eq!(success.aggregate["count"], 0, "{:#}", success.aggregate);
}

#[test]
fn inspect_dead_code_keeps_type_match_constructor_live_without_rescuing_dead_new_symbols() {
    let (_temp_dir, root, paths) = fixture_project(&[
        (
            "src/main.rs",
            r#"mod factory;
mod live_widget;
mod planted_dead;

fn main() {
    let _ = factory::make_live();
}
"#,
        ),
        (
            "src/factory.rs",
            r#"use crate::live_widget::LiveWidget;

pub fn make_live() -> LiveWidget {
    LiveWidget::new()
}
"#,
        ),
        (
            "src/live_widget.rs",
            r#"pub struct LiveWidget;

impl LiveWidget {
    pub fn new() -> Self { Self }
}
"#,
        ),
        (
            "src/planted_dead.rs",
            r#"pub fn orphan_function() {}

struct NeverConstructed;

impl NeverConstructed {
    pub fn new() -> Self { Self }
}
"#,
        ),
    ]);
    let project_root = std::fs::canonicalize(&root).expect("canonical fixture root");

    let mut first_snapshot = projected_snapshot_from_store(&project_root, &paths, ".store-one");
    first_snapshot.exported_symbols = type_match_fixture_exports(&project_root);
    let first_outbound = outbound_call_set_bytes(&first_snapshot);
    let first_scope_files = first_snapshot.files.clone();
    let first_success = scan(job(&project_root, first_scope_files, Some(first_snapshot)));

    let mut second_snapshot = projected_snapshot_from_store(&project_root, &paths, ".store-two");
    second_snapshot.exported_symbols = type_match_fixture_exports(&project_root);
    let second_outbound = outbound_call_set_bytes(&second_snapshot);
    let second_scope_files = second_snapshot.files.clone();
    let second_success = scan(job(
        &project_root,
        second_scope_files,
        Some(second_snapshot),
    ));

    assert_eq!(
        first_outbound, second_outbound,
        "projected outbound-call set must be byte-identical across cold builds"
    );
    assert_eq!(
        first_success.aggregate["count"], second_success.aggregate["count"],
        "dead-code count must be deterministic across cold builds"
    );

    assert!(
        !aggregate_has_item(&first_success, "src/live_widget.rs", "new"),
        "LiveWidget::new is reached only through a type_match edge and must not be dead: {:#}",
        first_success.aggregate
    );
    assert!(
        aggregate_has_item(&first_success, "src/planted_dead.rs", "orphan_function"),
        "genuinely-dead pub fn should remain reported: {:#}",
        first_success.aggregate
    );
    assert!(
        aggregate_has_item(&first_success, "src/planted_dead.rs", "new"),
        "genuinely-dead constructor should remain reported: {:#}",
        first_success.aggregate
    );
}

#[test]
fn rust_value_refs_and_cfg_test_callers_are_cold_deterministic() {
    let (_temp_dir, root, paths) = fixture_project(&[
        (
            "src/main.rs",
            r#"mod handlers;

fn main() {
    run();
}

fn run() {
    let lease = Some(());
    let _ = lease.map(crate::handlers::value_live);
    let _ = crate::handlers::NotCallback;
}
"#,
        ),
        (
            "src/handlers.rs",
            r#"pub struct NotCallback;
pub fn value_live(_: ()) {}
pub fn planted_dead() {}
pub fn shadowed() {}
pub fn shadow_parameter(shadowed: fn()) { consume(shadowed); }
pub fn shadow_pattern(maybe: Option<fn()>) {
    if let Some(shadowed) = maybe { consume(shadowed); }
}
pub fn test_only_write() {}
pub fn test_file_only() {}
fn consume(_: fn()) {}

#[cfg(test)]
mod tests {
    #[test]
    fn writes_only_in_tests() {
        crate::handlers::test_only_write();
    }
}
"#,
        ),
        (
            "src/handlers_test.rs",
            r#"fn test_file_caller() {
    crate::handlers::test_file_only();
}
"#,
        ),
    ]);
    let project_root = std::fs::canonicalize(&root).expect("canonical fixture root");

    let first_snapshot = projected_snapshot_from_store(&project_root, &paths, ".store-one");
    let first_outbound = outbound_call_set_bytes(&first_snapshot);
    let first_success = scan(job(
        &project_root,
        first_snapshot.files.clone(),
        Some(first_snapshot.clone()),
    ));

    let second_snapshot = projected_snapshot_from_store(&project_root, &paths, ".store-two");
    let second_outbound = outbound_call_set_bytes(&second_snapshot);
    let second_success = scan(job(
        &project_root,
        second_snapshot.files.clone(),
        Some(second_snapshot),
    ));

    assert_eq!(
        first_outbound, second_outbound,
        "projected refs must be byte-identical across cold builds"
    );
    assert_eq!(
        first_success.aggregate["count"], second_success.aggregate["count"],
        "headline dead-code count must be deterministic across cold builds"
    );
    assert_eq!(
        first_success.aggregate["test_only_count"], second_success.aggregate["test_only_count"],
        "test-only dead-code count must be deterministic across cold builds"
    );

    let value_target = target(&project_root, "src/handlers.rs", "value_live");
    let test_only_target = target(&project_root, "src/handlers.rs", "test_only_write");
    assert!(
        first_snapshot
            .outbound_calls
            .iter()
            .any(|call| call.target == test_only_target
                && call.caller_symbol.ends_with("writes_only_in_tests")),
        "cfg(test) caller must be present in the projected graph: {:#?}",
        first_snapshot.outbound_calls
    );
    let handler_facts = first_success
        .contributions
        .iter()
        .find(|contribution| contribution.contribution["file"] == "src/handlers.rs")
        .expect("handler contribution");
    assert!(
        handler_facts.contribution["cfg_test_ranges"]
            .as_array()
            .is_some_and(|ranges| !ranges.is_empty()),
        "cfg(test) source ranges must be persisted: {:#}",
        handler_facts.contribution
    );
    assert!(
        first_snapshot.outbound_calls.iter().any(|call| {
            call.target == value_target
                && call.caller_symbol == "run"
                && call.provenance == "value_ref"
        }),
        "function-as-value reference must project as a resolved value_ref: {:#?}",
        first_snapshot.outbound_calls
    );
    assert!(
        first_snapshot
            .outbound_calls
            .iter()
            .all(|call| { call.provenance != "value_ref" || !call.target.contains("NotCallback") }),
        "value paths that resolve to non-function items must not project: {:#?}",
        first_snapshot.outbound_calls
    );
    assert!(
        !aggregate_has_item(&first_success, "src/handlers.rs", "value_live"),
        "value-position referenced function must be live: {:#}",
        first_success.aggregate
    );
    assert!(
        aggregate_has_item(&first_success, "src/handlers.rs", "planted_dead"),
        "genuinely dead function is the planted negative control: {:#}",
        first_success.aggregate
    );
    assert!(
        aggregate_has_item(&first_success, "src/handlers.rs", "shadowed"),
        "a shadowing parameter must not create a value_ref edge to the function item: {:#}",
        first_success.aggregate
    );
    assert!(
        !aggregate_has_item(&first_success, "src/handlers.rs", "test_only_write"),
        "cfg(test)-only target must leave the headline bucket: {:#}",
        first_success.aggregate
    );
    let test_only = aggregate_test_only_item(&first_success, "src/handlers.rs", "test_only_write")
        .unwrap_or_else(|| {
            panic!(
                "missing cfg(test)-only finding: {:#}",
                first_success.aggregate
            )
        });
    assert_eq!(test_only["used_by"], json!(["src/handlers.rs:17"]));
    let test_file_only =
        aggregate_test_only_item(&first_success, "src/handlers.rs", "test_file_only")
            .unwrap_or_else(|| {
                panic!(
                    "missing test-file-only finding: {:#}",
                    first_success.aggregate
                )
            });
    assert_eq!(test_file_only["used_by"], json!(["src/handlers_test.rs"]));
}

#[test]
fn inspect_dead_code_keeps_go_sort_interface_methods_live() {
    let (_temp_dir, root, paths) = fixture_project(&[(
        "src/main.go",
        r#"package main

import "sort"

type sortable []int

func main() {
	values := sortable{3, 1, 2}
	sort.Sort(values)
}

func (s sortable) Len() int { return len(s) }

func (s sortable) Less(i, j int) bool { return s[i] < s[j] }

func (s sortable) Swap(i, j int) { s[i], s[j] = s[j], s[i] }

func (s sortable) Unused() {}

func Less(i, j int) bool { return false }
"#,
    )]);
    let project_root = std::fs::canonicalize(&root).expect("canonical fixture root");

    let first_snapshot = projected_snapshot_from_store(&project_root, &paths, ".go-store-one");
    let first_outbound = outbound_call_set_bytes(&first_snapshot);
    let first_scope_files = first_snapshot.files.clone();
    let first_success = scan(job(&project_root, first_scope_files, Some(first_snapshot)));

    let second_snapshot = projected_snapshot_from_store(&project_root, &paths, ".go-store-two");
    let second_outbound = outbound_call_set_bytes(&second_snapshot);
    let second_scope_files = second_snapshot.files.clone();
    let second_success = scan(job(
        &project_root,
        second_scope_files,
        Some(second_snapshot),
    ));

    assert_eq!(
        first_outbound, second_outbound,
        "Go sort.Interface fixture outbound-call set must be byte-identical across cold builds"
    );
    assert_eq!(
        first_success.aggregate["count"], second_success.aggregate["count"],
        "Go sort.Interface fixture dead-code count must be deterministic across cold builds"
    );

    for method in ["Len", "Less", "Swap"] {
        assert!(
            !aggregate_has_item_kind(&first_success, "src/main.go", method, "method"),
            "{method} is invoked by sort.Sort through sort.Interface and must be live: {:#}",
            first_success.aggregate
        );
    }
    assert!(
        aggregate_has_item_kind(&first_success, "src/main.go", "Unused", "method"),
        "unreferenced Go method should remain dead: {:#}",
        first_success.aggregate
    );
    assert!(
        aggregate_has_item_kind(&first_success, "src/main.go", "Less", "function"),
        "dispatch-name liveness must not rescue a same-named plain function: {:#}",
        first_success.aggregate
    );
}

#[test]
fn inspect_dead_code_ignored_manifest_does_not_suppress_fallback_entry_points() {
    let (_temp_dir, root, _paths) = fixture_project(&[
        (".gitignore", "fixtures/\n"),
        ("fixtures/pkg/package.json", r#"{"main":"src/index.ts"}"#),
        (
            "fixtures/pkg/src/index.ts",
            "export function ignoredPublicApi() { return 1; }\n",
        ),
        (
            "src/index.ts",
            "export function publicApi() { return 1; }\n",
        ),
    ]);
    let source_files = vec![root.join("src/index.ts")];
    let graph = snapshot(
        source_files.clone(),
        vec![export(&root, "src/index.ts", "publicApi", "function", 1)],
        Vec::new(),
        Vec::new(),
    );

    let success = scan(job(&root, source_files, Some(graph)));

    assert_eq!(success.aggregate["count"], 0, "{:#}", success.aggregate);
}

#[test]
fn inspect_dead_code_retains_full_drill_down_past_one_hundred_items() {
    let source = (0..101)
        .map(|index| format!("export function unused_{index}() {{}}\n"))
        .collect::<String>();
    let (_temp_dir, root, paths) = fixture_project(&[("src/many.ts", &source)]);
    let exports = (0..101)
        .map(|index| {
            export(
                &root,
                "src/many.ts",
                &format!("unused_{index}"),
                "function",
                index + 1,
            )
        })
        .collect::<Vec<_>>();
    let graph = snapshot(paths.clone(), exports, Vec::new(), Vec::new());

    let success = scan(job(&root, paths, Some(graph)));

    assert_eq!(success.aggregate["count"], 101);
    assert_eq!(success.aggregate["by_language"]["typescript"], 101);
    assert_eq!(
        success.aggregate["items"].as_array().expect("items").len(),
        101
    );
    assert_eq!(success.aggregate["drill_down_capped"], false);
    let symbols = success.aggregate["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["symbol"].clone())
        .collect::<Vec<_>>();
    assert_eq!(
        symbols,
        (0..101)
            .map(|index| json!(format!("unused_{index}")))
            .collect::<Vec<_>>()
    );
}

#[test]
fn inspect_dead_code_contributions_are_byte_identical_for_mixed_fixture() {
    let (_temp_dir, root, paths) = fixture_project(&[
        (
            "src/app.ts",
            "import { Service } from './service';\nexport function main(service: Service) { service.render(); }\n",
        ),
        (
            "src/service.ts",
            "export class Service { render(): Result { return {} as Result; } }\nexport interface Result { ok: boolean; }\n",
        ),
        ("src/barrel.ts", "export { Result } from './service';\n"),
        (
            "src/lib.rs",
            "pub use foo::Foo;\nmod foo;\npub fn use_foo(value: Foo) {}\n",
        ),
        ("src/foo.rs", "pub struct Foo;\npub struct Dead;\n"),
    ]);
    let graph = snapshot(
        paths.clone(),
        vec![
            export(&root, "src/app.ts", "main", "function", 2),
            export(&root, "src/service.ts", "Service", "class", 1),
            export(&root, "src/service.ts", "render", "method", 1),
            export(&root, "src/service.ts", "Result", "interface", 2),
            export(&root, "src/barrel.ts", "Result", "re_export", 1),
            export(&root, "src/lib.rs", "Foo", "struct", 1),
            export(&root, "src/lib.rs", "use_foo", "function", 3),
            export(&root, "src/foo.rs", "Foo", "struct", 1),
            export(&root, "src/foo.rs", "Dead", "struct", 2),
        ],
        vec![outbound(
            &root,
            "src/app.ts",
            "main",
            &dispatched_target("render", "service.render"),
            2,
        )],
        vec![root.join("src/app.ts")],
    );

    let success = scan(job(&root, paths, Some(graph)));
    let actual = contribution_bytes(&success);
    let expected = serde_json::to_string(&vec![
        (
            "src/app.ts".to_string(),
            json!({
                "file": "src/app.ts",
                "facts_format_version": 6,
                "generated": false,
                "exports": [
                    {"symbol": "main", "kind": "function", "line": 2}
                ],
                "raw_imports": [
                    {"source": "./service", "names": ["Service"], "default_import": null, "namespace_import": null}
                ],
                "type_ref_names": ["Service"]
            }),
        ),
        (
            "src/barrel.ts".to_string(),
            json!({
                "file": "src/barrel.ts",
                "facts_format_version": 6,
                "generated": false,
                "exports": [
                    {"symbol": "Result", "kind": "re_export", "line": 1}
                ],
                "raw_reexports": [
                    {"language": "ts", "source": "./service", "kind": "named", "imported": "Result", "exported": "Result", "line": 1}
                ]
            }),
        ),
        (
            "src/foo.rs".to_string(),
            json!({
                "file": "src/foo.rs",
                "facts_format_version": 6,
                "generated": false,
                "exports": [
                    {"symbol": "Foo", "kind": "struct", "line": 1, "is_type_like": true},
                    {"symbol": "Dead", "kind": "struct", "line": 2, "is_type_like": true}
                ],
                "rust_api": {
                    "items": [
                        {"name": "Foo", "scope": [], "public": true, "line": 1, "owner": null, "trait_name": null},
                        {"name": "Dead", "scope": [], "public": true, "line": 2, "owner": null, "trait_name": null}
                    ], "modules": [], "uses": []
                }
            }),
        ),
        (
            "src/lib.rs".to_string(),
            json!({
                "file": "src/lib.rs",
                "facts_format_version": 6,
                "generated": false,
                "exports": [
                    {"symbol": "Foo", "kind": "struct", "line": 1, "is_type_like": true},
                    {"symbol": "use_foo", "kind": "function", "line": 3}
                ],
                "rust_api": {
                    "items": [{"name": "use_foo", "scope": [], "public": true, "line": 3, "owner": null, "trait_name": null}],
                    "modules": [{"scope": [], "name": "foo", "public": false, "file": "src/foo.rs"}],
                    "uses": [{"scope": [], "path": ["foo", "Foo"], "alias": "Foo", "public": true, "line": 1}]
                },
                "raw_reexports": [
                    {"language": "rust", "source": "foo", "kind": "named", "imported": "Foo", "exported": "Foo", "line": 1}
                ],
                "rust_imports": [
                    {"source": "foo::Foo", "names": [], "default_import": null, "namespace_import": null}
                ],
                "type_ref_names": ["Foo"]
            }),
        ),
        (
            "src/service.ts".to_string(),
            json!({
                "file": "src/service.ts",
                "facts_format_version": 6,
                "generated": false,
                "exports": [
                    {"symbol": "Service", "kind": "class", "line": 1},
                    {"symbol": "render", "kind": "method", "line": 1},
                    {"symbol": "Result", "kind": "interface", "line": 2, "is_type_like": true}
                ],
                "type_ref_names": ["Result"]
            }),
        ),
    ])
    .expect("serialize expected contribution rows");

    assert_eq!(actual, expected);
}

#[test]
fn inspect_dead_code_contribution_shape_matches_contract() {
    let (_temp_dir, root, paths) = fixture_project(&[
        (
            "src/foo.ts",
            "export class Foo {}\nexport function helper() { return Bar(); }\n",
        ),
        ("src/bar.ts", "export function Bar() {}\n"),
    ]);
    let graph = snapshot(
        paths.clone(),
        vec![
            export(&root, "src/foo.ts", "Foo", "class", 1),
            export(&root, "src/foo.ts", "helper", "function", 2),
            export(&root, "src/bar.ts", "Bar", "function", 1),
        ],
        vec![
            outbound(
                &root,
                "src/foo.ts",
                "helper",
                &target(&root, "src/bar.ts", "Bar"),
                2,
            ),
            outbound(&root, "src/foo.ts", "helper", "external_dependency", 3),
        ],
        Vec::new(),
    );

    let success = scan(job(&root, paths, Some(graph)));
    let contribution = success
        .contributions
        .iter()
        .find(|contribution| contribution.file_path == root.join("src/foo.ts"))
        .expect("foo contribution");

    assert_eq!(
        contribution.contribution,
        json!({
            "file": "src/foo.ts",
            "facts_format_version": 6,
                "generated": false,
            "exports": [
                {"symbol": "Foo", "kind": "class", "line": 1},
                {"symbol": "helper", "kind": "function", "line": 2}
            ]
        })
    );
}
