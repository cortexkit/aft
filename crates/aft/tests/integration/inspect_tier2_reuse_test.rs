use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

#[cfg(debug_assertions)]
use aft::cache_freshness;
use aft::callgraph_store::CallGraphStore;
use aft::config::Config;
use aft::inspect::{
    InspectCache, InspectCategory, InspectManager, InspectScanSuccess, InspectSnapshot,
};
use aft::parser::SymbolCache;
use serde_json::Value;

fn write_file(root: &Path, relative: &str, contents: &str) -> PathBuf {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().expect("fixture file has parent")).expect("create parent");
    fs::write(&path, contents).expect("write fixture");
    path
}

fn snapshot(project_root: &Path, inspect_dir: &Path) -> InspectSnapshot {
    let config = Config {
        project_root: Some(project_root.to_path_buf()),
        ..Config::default()
    };
    InspectSnapshot::new(
        project_root.to_path_buf(),
        inspect_dir.to_path_buf(),
        Arc::new(config),
        Arc::new(RwLock::new(SymbolCache::new())),
    )
}

fn duplicate_source() -> String {
    r#"
export function calculate(input: number) {
  const first = input + 1;
  const second = first + 2;
  const third = second + first;
  const fourth = third + 3;
  const fifth = fourth + third;
  const sixth = fifth + second;
  const seventh = sixth + fifth;
  return seventh + fourth;
}
"#
    .to_string()
}

fn changed_source() -> String {
    r#"
export function calculate(input: number) {
  const first = input + 10;
  const second = first + 20;
  const third = second + first;
  const fourth = third + 30;
  const fifth = fourth + third;
  const sixth = fifth + second;
  const seventh = sixth + fifth;
  return seventh + fourth;
}
"#
    .to_string()
}

fn build_fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let root = temp_dir.path().join("project");
    fs::create_dir_all(&root).expect("create project");
    let source = duplicate_source();
    let mut mutated_file = PathBuf::new();
    for index in 0..32 {
        let relative = format!("src/file_{index:02}.ts");
        let path = write_file(&root, &relative, &source);
        if index == 7 {
            mutated_file = path;
        }
    }
    (temp_dir, root, mutated_file)
}

fn run_reuse(
    manager: &InspectManager,
    snapshot: InspectSnapshot,
) -> (InspectScanSuccess, Duration) {
    run_reuse_category(manager, snapshot, InspectCategory::Duplicates)
}

fn run_reuse_category(
    manager: &InspectManager,
    snapshot: InspectSnapshot,
    category: InspectCategory,
) -> (InspectScanSuccess, Duration) {
    let started = Instant::now();
    let result = manager.tier2_run_with_reuse_result(snapshot, category, None);
    let elapsed = started.elapsed();
    (result.outcome.expect("tier2 reuse run succeeds"), elapsed)
}

fn relative_paths(project_root: &Path, files: &[PathBuf]) -> Vec<String> {
    files
        .iter()
        .map(|file| {
            file.strip_prefix(project_root)
                .unwrap_or(file)
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect()
}

fn project_source_files(project_root: &Path) -> Vec<PathBuf> {
    fn visit(dir: &Path, files: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("."))
            {
                continue;
            }
            if path.is_dir() {
                visit(&path, files);
            } else if path
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| {
                    matches!(
                        ext,
                        "ts" | "tsx" | "js" | "jsx" | "mts" | "cts" | "mjs" | "cjs" | "rs"
                    )
                })
            {
                files.push(path);
            }
        }
    }

    let mut files = Vec::new();
    visit(project_root, &mut files);
    files.sort();
    files
}

fn artifact_cache_key_for_test(project_root: &Path) -> String {
    let _git_env = crate::test_helpers::hermetic_git_env_guard();
    aft::search_index::artifact_cache_key(project_root)
}

fn rebuild_dead_code_callgraph_store(project_root: &Path, inspect_dir: &Path) {
    let project_key = artifact_cache_key_for_test(project_root);
    let store_dir = inspect_dir
        .parent()
        .expect("inspect dir has parent")
        .join("callgraph")
        .join(&project_key);
    aft::root_cache::configure_artifact_access(project_root, &project_key, false);
    let store = CallGraphStore::open(store_dir, project_root.to_path_buf()).expect("open store");
    let files = project_source_files(project_root);
    store
        .cold_build(&files)
        .expect("cold build callgraph store");
}

fn contribution_payloads(
    project_root: &Path,
    success: &InspectScanSuccess,
) -> Vec<(String, Value)> {
    let mut payloads = success
        .contributions
        .iter()
        .map(|contribution| {
            let relative = contribution
                .file_path
                .strip_prefix(project_root)
                .unwrap_or(&contribution.file_path)
                .to_string_lossy()
                .replace('\\', "/");
            (relative, contribution.contribution.clone())
        })
        .collect::<Vec<_>>();
    payloads.sort_by(|left, right| left.0.cmp(&right.0));
    payloads
}

fn aggregate_item_symbols(success: &InspectScanSuccess) -> Vec<(String, String)> {
    let mut symbols = success.aggregate["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| {
            Some((
                item.get("file")?.as_str()?.to_string(),
                item.get("symbol")?.as_str()?.to_string(),
            ))
        })
        .collect::<Vec<_>>();
    symbols.sort();
    symbols
}

fn aggregate_contains_symbol(
    success: &InspectScanSuccess,
    file_suffix: &str,
    symbol: &str,
) -> bool {
    aggregate_item_symbols(success)
        .iter()
        .any(|(file, item_symbol)| {
            file.replace('\\', "/").ends_with(file_suffix) && item_symbol == symbol
        })
}

#[test]
fn blocking_dead_code_builds_a_cold_callgraph_snapshot() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let root = temp_dir.path().join("blocking-dead-code");
    fs::create_dir_all(&root).expect("create project");
    write_file(&root, "package.json", r#"{"main":"src/main.ts"}"#);
    write_file(
        &root,
        "src/main.ts",
        "export function entry(): void {}\nexport function plantedDead(): void {}\n",
    );
    let inspect_dir = temp_dir.path().join("cache/inspect");
    let project_key = artifact_cache_key_for_test(&root);
    aft::root_cache::configure_artifact_access(&root, &project_key, false);
    let manager = Arc::new(InspectManager::new());

    let outcome = manager.tier2_run_with_reuse_blocking_fresh(
        snapshot(&root, &inspect_dir),
        InspectCategory::DeadCode,
        aft::inspect::JobScope::for_project(root),
    );
    let payload = outcome.payload().expect("blocking scan is fresh");

    assert_eq!(payload["callgraph_available"], true);
}

#[test]
fn inspect_dead_code_framework_route_exports_are_live_but_route_helpers_remain_dead() {
    struct Case {
        name: &'static str,
        package_json: &'static str,
        route_file: &'static str,
        route_source: &'static str,
        live_symbol: &'static str,
    }

    let cases = [
        Case {
            name: "next",
            package_json: r#"{ "dependencies": { "next": "latest" } }"#,
            route_file: "app/api/route.ts",
            route_source: "import { liveDependency } from './service';
export function GET() { return liveDependency(); }
export function privateHelper() { return 0; }
",
            live_symbol: "GET",
        },
        Case {
            name: "nuxt",
            package_json: r#"{ "dependencies": { "nuxt": "latest" } }"#,
            route_file: "server/api/hello.ts",
            route_source: "import { liveDependency } from './service';
export default function handler() { return liveDependency(); }
export function privateHelper() { return 0; }
",
            live_symbol: "default",
        },
        Case {
            name: "sveltekit",
            package_json: r#"{ "devDependencies": { "@sveltejs/kit": "latest" }, "scripts": { "build": "vite build" } }"#,
            route_file: "src/routes/+server.ts",
            route_source: "import { liveDependency } from './service';
export function GET() { return liveDependency(); }
export function privateHelper() { return 0; }
",
            live_symbol: "GET",
        },
        Case {
            name: "remix",
            package_json: r#"{ "dependencies": { "@remix-run/react": "latest" } }"#,
            route_file: "app/routes/home.tsx",
            route_source: "import { liveDependency } from './service';
export async function loader() { return liveDependency(); }
export function privateHelper() { return 0; }
",
            live_symbol: "loader",
        },
        Case {
            name: "astro",
            package_json: r#"{ "dependencies": { "astro": "latest" } }"#,
            route_file: "src/pages/api.ts",
            route_source: "import { liveDependency } from './service';
export function GET() { return liveDependency(); }
export function privateHelper() { return 0; }
",
            live_symbol: "GET",
        },
    ];

    for case in cases {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let root = temp_dir
            .path()
            .join(format!("framework-route-{}", case.name));
        fs::create_dir_all(&root).expect("create project");
        write_file(&root, "package.json", case.package_json);
        write_file(&root, case.route_file, case.route_source);
        let service_file = Path::new(case.route_file)
            .parent()
            .expect("route file has parent")
            .join("service.ts")
            .to_string_lossy()
            .replace('\\', "/");
        write_file(
            &root,
            &service_file,
            "export function liveDependency() { return 1; }\nexport function unusedDependency() { return 0; }\n",
        );

        let inspect_dir = temp_dir.path().join(format!("inspect-{}", case.name));
        rebuild_dead_code_callgraph_store(&root, &inspect_dir);
        let manager = InspectManager::new();
        let (success, _elapsed) = run_reuse_category(
            &manager,
            snapshot(&root, &inspect_dir),
            InspectCategory::DeadCode,
        );

        assert!(
            !aggregate_contains_symbol(&success, case.route_file, case.live_symbol),
            "{} framework export should be live: {:#?}",
            case.name,
            success.aggregate
        );
        assert!(
            !aggregate_contains_symbol(&success, &service_file, "liveDependency"),
            "{} route dependency should be reachable from the framework export: {:#?}",
            case.name,
            success.aggregate
        );
        assert!(
            aggregate_contains_symbol(&success, case.route_file, "privateHelper"),
            "{} non-framework route helper export should remain dead-checkable: {:#?}",
            case.name,
            success.aggregate
        );

        let (unused_success, _elapsed) = run_reuse_category(
            &manager,
            snapshot(&root, &inspect_dir),
            InspectCategory::UnusedExports,
        );
        assert!(
            !aggregate_contains_symbol(&unused_success, case.route_file, case.live_symbol),
            "{} framework export should be live for unused_exports: {:#?}",
            case.name,
            unused_success.aggregate
        );
        assert!(
            aggregate_contains_symbol(&unused_success, case.route_file, "privateHelper"),
            "{} non-framework route helper export should remain unused-checkable: {:#?}",
            case.name,
            unused_success.aggregate
        );
    }
}

#[test]
fn inspect_dead_code_nestjs_decorator_roots_are_binding_aware_and_targeted() {
    struct Case {
        name: &'static str,
        package_json: &'static str,
        controller_source: &'static str,
        expected_root: bool,
        extra_files: &'static [(&'static str, &'static str)],
        live_dependency_file: Option<&'static str>,
    }

    let cases = [
        Case {
            name: "controller",
            package_json: r#"{ "dependencies": { "@nestjs/common": "latest" } }"#,
            controller_source: "import { Controller } from '@nestjs/common';
import { liveDependency } from './service';

@Controller()
export class DecoratedEntryPoint {
  findAll() { return liveDependency(); }
}

export function deadSibling() { return 0; }
",
            expected_root: true,
            extra_files: &[(
                "src/service.ts",
                "export function liveDependency() { return 1; }\nexport function unusedDependency() { return 0; }\n",
            )],
            live_dependency_file: Some("src/service.ts"),
        },
        Case {
            name: "aliased-controller",
            package_json: r#"{ "dependencies": { "@nestjs/common": "latest" } }"#,
            controller_source: "import { Controller as C } from '@nestjs/common';

@C()
export class DecoratedEntryPoint {}

export function deadSibling() { return 0; }
",
            expected_root: true,
            extra_files: &[],
            live_dependency_file: None,
        },
        Case {
            name: "local-controller-name",
            package_json: r#"{ "dependencies": { "@nestjs/common": "latest" } }"#,
            controller_source: "import { Controller } from './local-decorators';

@Controller()
export class DecoratedEntryPoint {}

export function deadSibling() { return 0; }
",
            expected_root: false,
            extra_files: &[(
                "src/local-decorators.ts",
                "export function Controller() { return () => undefined; }\n",
            )],
            live_dependency_file: None,
        },
        Case {
            name: "dev-dependency-without-nest-script",
            package_json: r#"{ "devDependencies": { "@nestjs/common": "latest" }, "scripts": { "docs": "vitepress dev" } }"#,
            controller_source: "import { Controller } from '@nestjs/common';

@Controller()
export class DecoratedEntryPoint {}

export function deadSibling() { return 0; }
",
            expected_root: false,
            extra_files: &[],
            live_dependency_file: None,
        },
        Case {
            name: "no-nest-dependency",
            package_json: r#"{ "dependencies": {} }"#,
            controller_source: "import { Controller } from '@nestjs/common';

@Controller()
export class DecoratedEntryPoint {}

export function deadSibling() { return 0; }
",
            expected_root: false,
            extra_files: &[],
            live_dependency_file: None,
        },
        Case {
            name: "graphql-resolver",
            package_json: r#"{ "dependencies": { "@nestjs/common": "latest", "@nestjs/graphql": "latest" } }"#,
            controller_source: "import { Resolver } from '@nestjs/graphql';

@Resolver()
export class DecoratedEntryPoint {}

export function deadSibling() { return 0; }
",
            expected_root: true,
            extra_files: &[],
            live_dependency_file: None,
        },
    ];

    for case in cases {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let root = temp_dir
            .path()
            .join(format!("nestjs-decorator-{}", case.name));
        fs::create_dir_all(&root).expect("create project");
        write_file(&root, "package.json", case.package_json);
        let controller_file = "src/app.controller.ts";
        write_file(&root, controller_file, case.controller_source);
        for (relative, contents) in case.extra_files {
            write_file(&root, relative, contents);
        }

        let inspect_dir = temp_dir.path().join(format!("inspect-{}", case.name));
        rebuild_dead_code_callgraph_store(&root, &inspect_dir);
        let manager = InspectManager::new();
        let (success, _elapsed) = run_reuse_category(
            &manager,
            snapshot(&root, &inspect_dir),
            InspectCategory::DeadCode,
        );
        let has_controller_item =
            aggregate_contains_symbol(&success, controller_file, "DecoratedEntryPoint");
        assert_eq!(
            has_controller_item, !case.expected_root,
            "{} decorator root mismatch: {:#?}",
            case.name, success.aggregate
        );
        assert!(
            aggregate_contains_symbol(&success, controller_file, "deadSibling"),
            "{} undecorated sibling export should remain dead-checkable: {:#?}",
            case.name,
            success.aggregate
        );
        if let Some(file) = case.live_dependency_file {
            assert!(
                !aggregate_contains_symbol(&success, file, "liveDependency"),
                "{} controller method dependency should be reachable: {:#?}",
                case.name,
                success.aggregate
            );
            assert!(
                aggregate_contains_symbol(&success, file, "unusedDependency"),
                "{} unused service export should remain dead-checkable: {:#?}",
                case.name,
                success.aggregate
            );
        }

        let (unused_success, _elapsed) = run_reuse_category(
            &manager,
            snapshot(&root, &inspect_dir),
            InspectCategory::UnusedExports,
        );
        let has_unused_controller_item =
            aggregate_contains_symbol(&unused_success, controller_file, "DecoratedEntryPoint");
        assert_eq!(
            has_unused_controller_item, !case.expected_root,
            "{} unused_exports decorator root mismatch: {:#?}",
            case.name, unused_success.aggregate
        );
        assert!(
            aggregate_contains_symbol(&unused_success, controller_file, "deadSibling"),
            "{} undecorated sibling export should remain unused-checkable: {:#?}",
            case.name,
            unused_success.aggregate
        );
    }
}

fn cycle_items(success: &InspectScanSuccess) -> Vec<Value> {
    success.aggregate["items"]
        .as_array()
        .expect("cycle items")
        .clone()
}

fn cycle_chains(success: &InspectScanSuccess) -> Vec<String> {
    cycle_items(success)
        .iter()
        .filter_map(|item| item.get("cycle")?.as_str().map(str::to_string))
        .collect()
}

fn aggregate_item<'a>(
    success: &'a InspectScanSuccess,
    file: &str,
    symbol: &str,
) -> Option<&'a Value> {
    success.aggregate["items"].as_array()?.iter().find(|item| {
        item["file"]
            .as_str()
            .is_some_and(|item_file| item_file.replace('\\', "/").ends_with(file))
            && item["symbol"] == symbol
    })
}

fn aggregate_test_only_item<'a>(
    success: &'a InspectScanSuccess,
    file: &str,
    symbol: &str,
) -> Option<&'a Value> {
    success.aggregate["test_only_items"]
        .as_array()?
        .iter()
        .find(|item| {
            item["file"]
                .as_str()
                .is_some_and(|item_file| item_file.replace('\\', "/").ends_with(file))
                && item["symbol"] == symbol
        })
}

/// `used_by` names each referencing test file by its project-relative path,
/// the same form inspect output uses for every other file reference.
fn assert_used_by(item: &Value, relative_path: &str) {
    let used_by = item["used_by"]
        .as_array()
        .unwrap_or_else(|| panic!("missing used_by on item: {item:#?}"));
    assert!(
        used_by
            .iter()
            .any(|value| value.as_str() == Some(relative_path)),
        "missing used_by path {relative_path} in {used_by:#?}"
    );
}

fn assert_reexport_context(item: &Value, file: &str, exported_name: &str) {
    let contexts = item["also_reexported"]
        .as_array()
        .unwrap_or_else(|| panic!("missing also_reexported context on item: {item:#?}"));
    assert!(
        contexts.iter().any(|context| {
            context["file"]
                .as_str()
                .is_some_and(|context_file| context_file.replace('\\', "/").ends_with(file))
                && context["exported_name"] == exported_name
        }),
        "missing re-export context {file}:{exported_name} in {contexts:#?}"
    );
}

#[test]
fn inspect_unused_exports_oxc_reexport_alias_reports_canonical_once_with_context() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let root = temp_dir.path().join("unused-reexport-context");
    fs::create_dir_all(&root).expect("create project");
    write_file(
        &root,
        "src/target.ts",
        "export const forwarded = 1;\nexport const localOnly = 2;\n",
    );
    write_file(
        &root,
        "src/barrel.ts",
        "export { forwarded } from './target';\n",
    );

    let manager = InspectManager::new();
    let (success, _elapsed) = run_reuse_category(
        &manager,
        snapshot(&root, &temp_dir.path().join("unused-inspect")),
        InspectCategory::UnusedExports,
    );

    assert!(
        aggregate_item(&success, "src/barrel.ts", "forwarded").is_none(),
        "barrel forwarding binding should not be counted: {:#?}",
        success.aggregate["items"]
    );
    let item = aggregate_item(&success, "src/target.ts", "forwarded").unwrap_or_else(|| {
        panic!(
            "canonical target export should be reported: {:#?}",
            success.aggregate
        )
    });
    assert_reexport_context(item, "src/barrel.ts", "forwarded");
}

#[test]
fn inspect_unused_exports_splits_test_only_references_from_headline() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let root = temp_dir.path().join("unused-test-only-tier");
    fs::create_dir_all(&root).expect("create project");
    write_file(
        &root,
        "src/api.ts",
        "export function testOnly() {}
export function productUsed() {}
export function plantedDead() {}
",
    );
    write_file(
        &root,
        "src/use.ts",
        "import { productUsed } from './api';
productUsed();
",
    );
    write_file(
        &root,
        "src/api.test.ts",
        "import { testOnly } from './api';
testOnly();
",
    );
    write_file(
        &root,
        "src/barrel-target.ts",
        "export function throughBarrel() {}
export function barrelDead() {}
",
    );
    write_file(
        &root,
        "src/barrel.ts",
        "export { throughBarrel } from './barrel-target';
",
    );
    write_file(
        &root,
        "src/barrel.test.ts",
        "import { throughBarrel } from './barrel';
throughBarrel();
",
    );

    let manager = InspectManager::new();
    let (success, _elapsed) = run_reuse_category(
        &manager,
        snapshot(&root, &temp_dir.path().join("unused-test-only-inspect")),
        InspectCategory::UnusedExports,
    );

    assert_eq!(success.aggregate["count"], 2, "{:#}", success.aggregate);
    assert!(
        aggregate_item(&success, "src/api.ts", "plantedDead").is_some(),
        "{:#}",
        success.aggregate
    );
    assert!(aggregate_item(&success, "src/barrel-target.ts", "barrelDead").is_some());
    assert!(aggregate_item(&success, "src/api.ts", "testOnly").is_none());
    assert!(aggregate_item(&success, "src/api.ts", "productUsed").is_none());
    assert!(aggregate_item(&success, "src/barrel-target.ts", "throughBarrel").is_none());

    assert_eq!(
        success.aggregate["test_only_count"], 2,
        "{:#}",
        success.aggregate
    );
    let test_only = aggregate_test_only_item(&success, "src/api.ts", "testOnly")
        .unwrap_or_else(|| panic!("missing test-only item: {:#}", success.aggregate));
    assert_used_by(test_only, "src/api.test.ts");
    let through_barrel =
        aggregate_test_only_item(&success, "src/barrel-target.ts", "throughBarrel")
            .unwrap_or_else(|| panic!("missing barrel test-only item: {:#}", success.aggregate));
    assert_used_by(through_barrel, "src/barrel.test.ts");
}

#[test]
fn inspect_tier2_reuse_skips_fresh_files_and_rescans_stale_file() {
    let (temp_dir, root, mutated_file) = build_fixture();
    let inspect_dir = root.join(".aft-cache").join("inspect");

    let first_manager = InspectManager::new();
    let (first, _t1) = run_reuse(&first_manager, snapshot(&root, &inspect_dir));
    assert_eq!(first.scanned_files.len(), 32);
    assert!(first.aggregate["groups_count"].as_u64().unwrap_or(0) > 0);

    #[cfg(debug_assertions)]
    cache_freshness::reset_hash_file_if_small_count_for_debug();
    let second_manager = InspectManager::new();
    let (second, _t2) = run_reuse(&second_manager, snapshot(&root, &inspect_dir));
    // Cache reuse is proven behaviorally: a fully-fresh second run rescans
    // zero files and returns the identical aggregate. (A wall-clock "faster"
    // assertion was removed — it flaked under parallel test load while adding
    // no signal beyond the scanned_files/aggregate checks below.)
    assert!(second.scanned_files.is_empty());
    assert_eq!(second.aggregate, first.aggregate);
    #[cfg(debug_assertions)]
    assert_eq!(
        cache_freshness::hash_file_if_small_count_for_debug(),
        0,
        "fully fresh quick reuse must be stat-only and avoid source hashing"
    );

    fs::write(&mutated_file, changed_source()).expect("mutate one fixture file");

    let third_manager = InspectManager::new();
    let (third, _t3) = run_reuse(&third_manager, snapshot(&root, &inspect_dir));
    assert_eq!(
        relative_paths(&root, &third.scanned_files),
        vec!["src/file_07.ts"]
    );
    assert_ne!(third.aggregate, first.aggregate);

    let cold_inspect_dir = temp_dir.path().join("inspect-cold-duplicates-after-edit");
    let cold_manager = InspectManager::new();
    let (cold, _cold_elapsed) = run_reuse(&cold_manager, snapshot(&root, &cold_inspect_dir));
    assert_eq!(
        third.aggregate, cold.aggregate,
        "stat-diff incremental aggregate must match a cold hash-all scan after an mtime-advancing edit"
    );
}

#[test]
fn inspect_cycles_reports_import_sccs_once_and_excludes_type_only_edges() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let root = temp_dir.path().join("project-cycles");
    fs::create_dir_all(&root).expect("create project");
    write_file(
        &root,
        "src/two_a.ts",
        "import { twoB } from './two_b';\nexport const twoA = twoB;\n",
    );
    write_file(
        &root,
        "src/two_b.ts",
        "import { twoA } from './two_a';\nexport const twoB = twoA;\n",
    );
    write_file(
        &root,
        "src/three_a.ts",
        "import { threeB } from './three_b';\nexport const threeA = threeB;\n",
    );
    write_file(
        &root,
        "src/three_b.ts",
        "import { threeC } from './three_c';\nexport const threeB = threeC;\n",
    );
    write_file(
        &root,
        "src/three_c.ts",
        "import { threeA } from './three_a';\nexport const threeC = threeA;\n",
    );
    write_file(
        &root,
        "src/type_a.ts",
        "import type { TypeB } from './type_b';\nexport type TypeA = { b?: TypeB };\n",
    );
    write_file(
        &root,
        "src/type_b.ts",
        "import type { TypeA } from './type_a';\nexport type TypeB = { a?: TypeA };\n",
    );
    write_file(
        &root,
        "src/chain_a.ts",
        "import { chainB } from './chain_b';\nexport const chainA = chainB;\n",
    );
    write_file(
        &root,
        "src/chain_b.ts",
        "import { chainC } from './chain_c';\nexport const chainB = chainC;\n",
    );
    write_file(&root, "src/chain_c.ts", "export const chainC = 1;\n");

    let manager = InspectManager::new();
    let (success, _elapsed) = run_reuse_category(
        &manager,
        snapshot(&root, &root.join(".aft-cache").join("inspect")),
        InspectCategory::Cycles,
    );

    assert_eq!(success.aggregate["count"], 2);
    assert_eq!(success.aggregate["largest"], 3);
    assert_eq!(
        cycle_chains(&success),
        vec![
            "src/three_a.ts -> src/three_b.ts -> src/three_c.ts -> src/three_a.ts".to_string(),
            "src/two_a.ts -> src/two_b.ts -> src/two_a.ts".to_string(),
        ]
    );
    let rendered = success.aggregate.to_string();
    assert!(
        !rendered.contains("type_a.ts"),
        "type-only cycle must be ignored: {rendered}"
    );
    assert!(
        !rendered.contains("chain_a.ts"),
        "acyclic chain must be ignored: {rendered}"
    );
    assert!(
        cycle_items(&success).iter().all(|item| item
            .get("cycle")
            .and_then(Value::as_str)
            .is_some_and(|cycle| !cycle.contains('\\'))),
        "cycle display paths must use forward slashes: {:#}",
        success.aggregate
    );
    assert!(
        cycle_items(&success)
            .iter()
            .all(|item| item.get("edge_kind").and_then(Value::as_str) == Some("static")),
        "fixture cycles are static imports: {:#}",
        success.aggregate
    );
}

#[test]
fn inspect_cycles_incremental_rescans_only_changed_file() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let root = temp_dir.path().join("project-cycles-incremental");
    fs::create_dir_all(&root).expect("create project");
    let changed_file = write_file(
        &root,
        "src/a.ts",
        "import { b } from './b';\nexport const a = b;\n",
    );
    write_file(
        &root,
        "src/b.ts",
        "import { a } from './a';\nexport const b = a;\n",
    );
    let inspect_dir = root.join(".aft-cache").join("inspect");

    let first_manager = InspectManager::new();
    let (first, _elapsed) = run_reuse_category(
        &first_manager,
        snapshot(&root, &inspect_dir),
        InspectCategory::Cycles,
    );
    assert_eq!(first.aggregate["count"], 1);
    assert_eq!(relative_paths(&root, &first.scanned_files).len(), 2);

    fs::write(
        &changed_file,
        "// keep the import edge while changing this file's cached facts\nimport { b } from './b';\nexport const a = b;\n",
    )
    .expect("edit one cycle file");

    let second_manager = InspectManager::new();
    let (second, _elapsed) = run_reuse_category(
        &second_manager,
        snapshot(&root, &inspect_dir),
        InspectCategory::Cycles,
    );
    assert_eq!(
        relative_paths(&root, &second.scanned_files),
        vec!["src/a.ts"]
    );
    assert_eq!(second.aggregate["count"], first.aggregate["count"]);
    assert_eq!(cycle_chains(&second), cycle_chains(&first));
}

#[test]
fn inspect_tier2_reuse_rescans_mtime_advancing_same_size_change_and_matches_cold() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let root = temp_dir.path().join("project-mtime-advancing");
    fs::create_dir_all(&root).expect("create project");
    let source = write_file(&root, "src/export.ts", "export function one() {}\n");
    let initial_mtime = filetime::FileTime::from_unix_time(1_700_000_000, 0);
    filetime::set_file_mtime(&source, initial_mtime).expect("set initial mtime");
    let inspect_dir = root.join(".aft-cache").join("inspect");

    let warm_manager = InspectManager::new();
    let (first, _t1) = run_reuse_category(
        &warm_manager,
        snapshot(&root, &inspect_dir),
        InspectCategory::UnusedExports,
    );
    assert_eq!(first.scanned_files.len(), 1);
    assert_eq!(first.aggregate["items"][0]["symbol"], "one");

    fs::write(&source, "export function two() {}\n").expect("same-size mutate");
    let advanced_mtime = filetime::FileTime::from_unix_time(1_700_000_001, 0);
    filetime::set_file_mtime(&source, advanced_mtime).expect("advance mtime");

    let (warm, _t2) = run_reuse_category(
        &warm_manager,
        snapshot(&root, &inspect_dir),
        InspectCategory::UnusedExports,
    );
    assert_eq!(
        relative_paths(&root, &warm.scanned_files),
        vec!["src/export.ts"]
    );
    assert_eq!(warm.aggregate["items"][0]["symbol"], "two");
    assert_ne!(warm.aggregate, first.aggregate);

    let cold_inspect_dir = temp_dir.path().join("inspect-cold-mtime-advancing");
    let cold_manager = InspectManager::new();
    let (cold, _cold_elapsed) = run_reuse_category(
        &cold_manager,
        snapshot(&root, &cold_inspect_dir),
        InspectCategory::UnusedExports,
    );
    assert_eq!(
        warm.aggregate, cold.aggregate,
        "stat-diff incremental aggregate must match a cold hash-all scan for normal mtime-advancing edits"
    );
}

#[test]
fn inspect_tier2_reuse_treats_mtime_preserving_same_size_change_as_fresh_until_ceiling() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let root = temp_dir.path().join("project-mtime-preserving");
    fs::create_dir_all(&root).expect("create project");
    let source = write_file(&root, "src/export.ts", "export function one() {}\n");
    let fixed_mtime = filetime::FileTime::from_unix_time(1_700_000_000, 0);
    filetime::set_file_mtime(&source, fixed_mtime).expect("set fixed mtime");
    let inspect_dir = root.join(".aft-cache").join("inspect");

    let first_manager = InspectManager::new();
    let (first, _t1) = run_reuse_category(
        &first_manager,
        snapshot(&root, &inspect_dir),
        InspectCategory::UnusedExports,
    );
    assert_eq!(first.scanned_files.len(), 1);
    assert_eq!(first.aggregate["items"][0]["symbol"], "one");

    fs::write(&source, "export function two() {}\n").expect("same-size mutate");
    filetime::set_file_mtime(&source, fixed_mtime).expect("restore mtime");

    let second_manager = InspectManager::new();
    let (second, _t2) = run_reuse_category(
        &second_manager,
        snapshot(&root, &inspect_dir),
        InspectCategory::UnusedExports,
    );

    // Accepted stat-diff residual: same-size content changed while mtime was
    // preserved looks fresh, trading rare stale advisory Code Health for a
    // stat-only per-edit path. The 30-minute staleness ceiling's strict pass
    // heals this case; do not reintroduce per-edit hash-all to catch it here.
    assert!(second.scanned_files.is_empty());
    assert_eq!(second.aggregate, first.aggregate);
    assert_eq!(second.aggregate["items"][0]["symbol"], "one");

    let cold_inspect_dir = temp_dir.path().join("inspect-cold-mtime-preserving");
    let cold_manager = InspectManager::new();
    let (cold, _cold_elapsed) = run_reuse_category(
        &cold_manager,
        snapshot(&root, &cold_inspect_dir),
        InspectCategory::UnusedExports,
    );
    assert_eq!(cold.aggregate["items"][0]["symbol"], "two");
    assert_ne!(
        second.aggregate, cold.aggregate,
        "cold scan proves the preserved-mtime edit changed content even though stat-diff reused the cached aggregate"
    );
}

fn unused_contribution_payloads(
    project_root: &Path,
    success: &InspectScanSuccess,
) -> Vec<(String, Value)> {
    let mut payloads = success
        .contributions
        .iter()
        .map(|contribution| {
            let relative = contribution
                .file_path
                .strip_prefix(project_root)
                .unwrap_or(&contribution.file_path)
                .to_string_lossy()
                .replace('\\', "/");
            (relative, contribution.contribution.clone())
        })
        .collect::<Vec<_>>();
    payloads.sort_by(|left, right| left.0.cmp(&right.0));
    payloads
}

fn assert_unused_exports_incremental_matches_cold<S, E>(name: &str, setup: S, edit: E)
where
    S: FnOnce(&Path),
    E: FnOnce(&Path),
{
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let root = temp_dir.path().join(format!("project-{name}"));
    fs::create_dir_all(&root).expect("create project");
    setup(&root);

    let warm_inspect_dir = temp_dir.path().join(format!("inspect-warm-{name}"));
    let warm_manager = InspectManager::new();
    let (first, _first_elapsed) = run_reuse_category(
        &warm_manager,
        snapshot(&root, &warm_inspect_dir),
        InspectCategory::UnusedExports,
    );
    assert!(
        !first.contributions.is_empty(),
        "{name}: initial cold scan should populate contributions"
    );

    edit(&root);

    let (warm, _warm_elapsed) = run_reuse_category(
        &warm_manager,
        snapshot(&root, &warm_inspect_dir),
        InspectCategory::UnusedExports,
    );
    let cold_inspect_dir = temp_dir.path().join(format!("inspect-cold-{name}"));
    let cold_manager = InspectManager::new();
    let (cold, _cold_elapsed) = run_reuse_category(
        &cold_manager,
        snapshot(&root, &cold_inspect_dir),
        InspectCategory::UnusedExports,
    );

    assert_eq!(warm.aggregate, cold.aggregate, "{name}: aggregate mismatch");
    assert_eq!(
        unused_contribution_payloads(&root, &warm),
        unused_contribution_payloads(&root, &cold),
        "{name}: per-file contribution payload mismatch"
    );
}

#[test]
fn inspect_unused_exports_quick_reuse_invalidates_node_modules_tsconfig_change() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let root = temp_dir
        .path()
        .join("project-quick-reuse-node-modules-tsconfig");
    fs::create_dir_all(&root).expect("create project");
    write_file(
        &root,
        "tsconfig.json",
        r#"{"extends":"./node_modules/@scope/tsconfig/tsconfig.base.json"}"#,
    );
    write_file(
        &root,
        "node_modules/@scope/tsconfig/tsconfig.base.json",
        r#"{"compilerOptions":{"baseUrl":"../../..","paths":{"@lib":["src/a.ts"]}}}"#,
    );
    write_file(
        &root,
        "src/a.ts",
        "export const x = 'a';
export const onlyA = 'a-only';
",
    );
    write_file(
        &root,
        "src/b.ts",
        "export const x = 'b';
export const onlyB = 'b-only';
",
    );
    write_file(
        &root,
        "src/use.ts",
        "import { x } from '@lib';
console.log(x);
",
    );

    let warm_inspect_dir = temp_dir
        .path()
        .join("inspect-warm-quick-reuse-node-modules-tsconfig");
    let warm_manager = InspectManager::new();
    let (first, _first_elapsed) = run_reuse_category(
        &warm_manager,
        snapshot(&root, &warm_inspect_dir),
        InspectCategory::UnusedExports,
    );
    assert!(
        aggregate_contains_symbol(&first, "src/b.ts", "x"),
        "initial alias target should make src/b.ts::x unused"
    );

    write_file(
        &root,
        "node_modules/@scope/tsconfig/tsconfig.base.json",
        r#"{"compilerOptions":{"baseUrl":"../../..","paths":{"@lib":["src/b.ts"]}}}"#,
    );

    let (warm, _warm_elapsed) = run_reuse_category(
        &warm_manager,
        snapshot(&root, &warm_inspect_dir),
        InspectCategory::UnusedExports,
    );
    let cold_inspect_dir = temp_dir
        .path()
        .join("inspect-cold-quick-reuse-node-modules-tsconfig");
    let cold_manager = InspectManager::new();
    let (cold, _cold_elapsed) = run_reuse_category(
        &cold_manager,
        snapshot(&root, &cold_inspect_dir),
        InspectCategory::UnusedExports,
    );

    assert_eq!(
        warm.aggregate, cold.aggregate,
        "resolver-config-only edit should recompute the same roll-up as a cold scan"
    );
    assert_ne!(
        warm.aggregate, first.aggregate,
        "warm result must not reuse the stale pre-edit aggregate"
    );
    assert!(
        warm.scanned_files.is_empty(),
        "node_modules resolver-config edit should exercise the no-source-rescan reuse path"
    );
    assert!(
        !warm.contributions.is_empty(),
        "quick reuse should miss and roll up cached per-file contributions"
    );
    assert!(
        aggregate_contains_symbol(&warm, "src/a.ts", "x"),
        "updated alias target should make src/a.ts::x unused"
    );
}

#[test]
fn inspect_unused_exports_tracks_external_package_tsconfig_extends_change() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let repo = temp_dir.path().join("repo");
    let root = repo.join("pkg");
    fs::create_dir_all(&root).expect("create project");
    write_file(
        &root,
        "tsconfig.json",
        r#"{"extends":"../tsconfig.base.json"}"#,
    );
    write_file(
        &repo,
        "tsconfig.base.json",
        r#"{"extends":"@scope/tsconfig"}"#,
    );
    write_file(
        &repo,
        "node_modules/@scope/tsconfig/package.json",
        r#"{"name":"@scope/tsconfig","version":"1.0.0","main":"base.json","tsconfig":"base.json"}"#,
    );
    write_file(
        &repo,
        "node_modules/@scope/tsconfig/base.json",
        r#"{"compilerOptions":{"baseUrl":"../../..","paths":{"@lib":["pkg/src/a.ts"]}}}"#,
    );
    write_file(
        &root,
        "src/a.ts",
        "export const x = 'a';
export const onlyA = 'a-only';
",
    );
    write_file(
        &root,
        "src/b.ts",
        "export const x = 'b';
export const onlyB = 'b-only';
",
    );
    write_file(
        &root,
        "src/use.ts",
        "import { x } from '@lib';
console.log(x);
",
    );

    let warm_inspect_dir = temp_dir
        .path()
        .join("inspect-warm-external-package-extends");
    let warm_manager = InspectManager::new();
    let (first, _first_elapsed) = run_reuse_category(
        &warm_manager,
        snapshot(&root, &warm_inspect_dir),
        InspectCategory::UnusedExports,
    );
    assert!(
        aggregate_contains_symbol(&first, "src/b.ts", "x"),
        "initial package tsconfig alias target should make src/b.ts::x unused"
    );

    write_file(
        &repo,
        "node_modules/@scope/tsconfig/base.json",
        r#"{"compilerOptions":{"baseUrl":"../../..","paths":{"@lib":["pkg/src/b.ts"]}}}"#,
    );

    let (warm, _warm_elapsed) = run_reuse_category(
        &warm_manager,
        snapshot(&root, &warm_inspect_dir),
        InspectCategory::UnusedExports,
    );
    let cold_inspect_dir = temp_dir
        .path()
        .join("inspect-cold-external-package-extends");
    let cold_manager = InspectManager::new();
    let (cold, _cold_elapsed) = run_reuse_category(
        &cold_manager,
        snapshot(&root, &cold_inspect_dir),
        InspectCategory::UnusedExports,
    );

    assert_eq!(
        warm.aggregate, cold.aggregate,
        "external package tsconfig edit should recompute the same roll-up as a cold scan"
    );
    assert_ne!(
        warm.aggregate, first.aggregate,
        "warm result must include the second-order bare-package extends dependency"
    );
    assert!(
        warm.scanned_files.is_empty(),
        "external config edit should not require source rescans"
    );
    assert!(
        !warm.contributions.is_empty(),
        "quick reuse should miss and roll up cached per-file contributions"
    );
    assert!(
        aggregate_contains_symbol(&warm, "src/a.ts", "x"),
        "updated package tsconfig alias target should make src/a.ts::x unused"
    );
}

#[test]
fn inspect_unused_exports_tracks_external_package_tsconfig_package_json_field_change() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let repo = temp_dir.path().join("repo");
    let root = repo.join("pkg");
    fs::create_dir_all(&root).expect("create project");
    write_file(
        &root,
        "tsconfig.json",
        r#"{"extends":"../tsconfig.base.json"}"#,
    );
    write_file(
        &repo,
        "tsconfig.base.json",
        r#"{"extends":"@scope/tsconfig"}"#,
    );
    write_file(
        &repo,
        "node_modules/@scope/tsconfig/package.json",
        r#"{"name":"@scope/tsconfig","version":"1.0.0","main":"base-a.json","tsconfig":"base-a.json"}"#,
    );
    write_file(
        &repo,
        "node_modules/@scope/tsconfig/base-a.json",
        r#"{"compilerOptions":{"baseUrl":"../../..","paths":{"@lib":["pkg/src/a.ts"]}}}"#,
    );
    write_file(
        &repo,
        "node_modules/@scope/tsconfig/base-b.json",
        r#"{"compilerOptions":{"baseUrl":"../../..","paths":{"@lib":["pkg/src/b.ts"]}}}"#,
    );
    write_file(
        &root,
        "src/a.ts",
        "export const x = 'a';
export const onlyA = 'a-only';
",
    );
    write_file(
        &root,
        "src/b.ts",
        "export const x = 'b';
export const onlyB = 'b-only';
",
    );
    write_file(
        &root,
        "src/use.ts",
        "import { x } from '@lib';
console.log(x);
",
    );

    let warm_inspect_dir = temp_dir
        .path()
        .join("inspect-warm-external-package-json-tsconfig");
    let warm_manager = InspectManager::new();
    let (first, _first_elapsed) = run_reuse_category(
        &warm_manager,
        snapshot(&root, &warm_inspect_dir),
        InspectCategory::UnusedExports,
    );
    assert!(
        aggregate_contains_symbol(&first, "src/b.ts", "x"),
        "initial package.json tsconfig target should make src/b.ts::x unused"
    );

    write_file(
        &repo,
        "node_modules/@scope/tsconfig/package.json",
        r#"{"name":"@scope/tsconfig","version":"1.0.0","main":"base-b.json","tsconfig":"base-b.json"}"#,
    );

    let (warm, _warm_elapsed) = run_reuse_category(
        &warm_manager,
        snapshot(&root, &warm_inspect_dir),
        InspectCategory::UnusedExports,
    );
    let cold_inspect_dir = temp_dir
        .path()
        .join("inspect-cold-external-package-json-tsconfig");
    let cold_manager = InspectManager::new();
    let (cold, _cold_elapsed) = run_reuse_category(
        &cold_manager,
        snapshot(&root, &cold_inspect_dir),
        InspectCategory::UnusedExports,
    );

    assert_eq!(
        warm.aggregate, cold.aggregate,
        "package.json tsconfig field edit should recompute the same roll-up as a cold scan"
    );
    assert_ne!(
        warm.aggregate, first.aggregate,
        "warm result must include the package.json-selected resolver config"
    );
    assert!(
        warm.scanned_files.is_empty(),
        "package.json resolver-config edit should not require source rescans"
    );
    assert!(
        !warm.contributions.is_empty(),
        "quick reuse should miss and roll up cached per-file contributions"
    );
    assert!(
        aggregate_contains_symbol(&warm, "src/a.ts", "x"),
        "updated package.json tsconfig target should make src/a.ts::x unused"
    );
}

#[test]
fn inspect_unused_exports_hash_tracks_package_json_tsconfig_dependencies() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let repo = temp_dir.path().join("repo");
    let root = repo.join("pkg");
    fs::create_dir_all(&root).expect("create project");
    write_file(
        &root,
        "tsconfig.json",
        r#"{"extends":"../tsconfig.base.json"}"#,
    );
    write_file(
        &repo,
        "tsconfig.base.json",
        r#"{"extends":"@scope/tsconfig"}"#,
    );
    write_file(
        &repo,
        "node_modules/@scope/tsconfig/package.json",
        r#"{"name":"@scope/tsconfig","version":"1.0.0","tsconfig":"base.json"}"#,
    );
    write_file(
        &repo,
        "node_modules/@scope/tsconfig/base.json",
        r#"{"compilerOptions":{"baseUrl":"../../..","paths":{"@lib":["pkg/src/a.ts"]}}}"#,
    );

    let cache = InspectCache::open(
        temp_dir.path().join("inspect-hash-package-json-tsconfig"),
        root,
    )
    .expect("open cache");
    let first = cache
        .contribution_set_hash(InspectCategory::UnusedExports)
        .expect("initial hash");

    write_file(
        &repo,
        "node_modules/@scope/tsconfig/base.json",
        r#"{"compilerOptions":{"baseUrl":"../../..","paths":{"@lib":["pkg/src/b.ts"]}}}"#,
    );
    let selected_config_changed = cache
        .contribution_set_hash(InspectCategory::UnusedExports)
        .expect("selected config hash");
    assert_ne!(
        selected_config_changed, first,
        "package.json-selected tsconfig edits must invalidate contribution hashes"
    );

    write_file(
        &repo,
        "node_modules/@scope/tsconfig/alternate.json",
        r#"{"compilerOptions":{"baseUrl":"../../..","paths":{"@lib":["pkg/src/a.ts"]}}}"#,
    );
    write_file(
        &repo,
        "node_modules/@scope/tsconfig/package.json",
        r#"{"name":"@scope/tsconfig","version":"1.0.0","tsconfig":"alternate.json"}"#,
    );
    let package_json_changed = cache
        .contribution_set_hash(InspectCategory::UnusedExports)
        .expect("package.json hash");
    assert_ne!(
        package_json_changed, selected_config_changed,
        "package.json tsconfig field edits must invalidate contribution hashes"
    );
}

fn assert_dead_code_incremental_matches_cold<S, E>(
    name: &str,
    setup: S,
    edit: E,
    expected_scanned_after_edit: &[&str],
) where
    S: FnOnce(&Path),
    E: FnOnce(&Path),
{
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let root = temp_dir.path().join(format!("dead-code-project-{name}"));
    fs::create_dir_all(&root).expect("create project");
    setup(&root);

    let warm_inspect_dir = temp_dir.path().join(format!("warm-{name}/inspect"));
    rebuild_dead_code_callgraph_store(&root, &warm_inspect_dir);
    let warm_manager = InspectManager::new();
    let (first, _first_elapsed) = run_reuse_category(
        &warm_manager,
        snapshot(&root, &warm_inspect_dir),
        InspectCategory::DeadCode,
    );
    assert_eq!(
        first.aggregate["callgraph_available"], true,
        "{name}: callgraph store should be available"
    );
    assert!(
        !first.contributions.is_empty(),
        "{name}: initial cold scan should populate contributions"
    );

    edit(&root);
    rebuild_dead_code_callgraph_store(&root, &warm_inspect_dir);

    let (warm, _warm_elapsed) = run_reuse_category(
        &warm_manager,
        snapshot(&root, &warm_inspect_dir),
        InspectCategory::DeadCode,
    );
    let cold_inspect_dir = temp_dir.path().join(format!("cold-{name}/inspect"));
    rebuild_dead_code_callgraph_store(&root, &cold_inspect_dir);
    let cold_manager = InspectManager::new();
    let (cold, _cold_elapsed) = run_reuse_category(
        &cold_manager,
        snapshot(&root, &cold_inspect_dir),
        InspectCategory::DeadCode,
    );

    assert_eq!(
        relative_paths(&root, &warm.scanned_files),
        expected_scanned_after_edit,
        "{name}: edited source reparses should stay minimal"
    );
    assert_eq!(warm.aggregate, cold.aggregate, "{name}: aggregate mismatch");
    assert_eq!(
        contribution_payloads(&root, &warm),
        contribution_payloads(&root, &cold),
        "{name}: raw per-file contribution payload mismatch"
    );
}

#[test]
fn inspect_dead_code_incremental_facts_invariants_match_cold() {
    assert_dead_code_incremental_matches_cold(
        "ts_test_only_import_removed",
        |root| {
            write_file(root, "package.json", r#"{"main":"src/main.ts"}"#);
            write_file(
                root,
                "src/main.ts",
                "export function main() {}
",
            );
            write_file(
                root,
                "src/api.ts",
                "export function testOnly() {}
export function plantedDead() {}
",
            );
            write_file(
                root,
                "src/api.test.ts",
                "import { testOnly } from './api';
testOnly();
",
            );
        },
        |root| {
            write_file(
                root,
                "src/api.test.ts",
                "console.log('import removed');
",
            );
        },
        &["src/api.test.ts"],
    );

    assert_dead_code_incremental_matches_cold(
        "ts_last_importer_removed",
        |root| {
            write_file(root, "package.json", r#"{"main":"src/main.ts"}"#);
            write_file(
                root,
                "src/exported.ts",
                "export const used = 1;\nexport const dead = 2;\n",
            );
            write_file(
                root,
                "src/main.ts",
                "import { used } from './exported';\nexport function main() { console.log(used); }\n",
            );
        },
        |root| {
            write_file(
                root,
                "src/main.ts",
                "export function main() { console.log('removed'); }\n",
            );
        },
        &["src/main.ts"],
    );

    assert_dead_code_incremental_matches_cold(
        "ts_importer_deleted",
        |root| {
            write_file(root, "package.json", r#"{"main":"src/main.ts"}"#);
            write_file(root, "src/exported.ts", "export const used = 1;\n");
            write_file(
                root,
                "src/main.ts",
                "import { used } from './exported';\nconsole.log(used);\n",
            );
        },
        |root| {
            fs::remove_file(root.join("src/main.ts")).expect("delete importer");
        },
        &[],
    );

    assert_dead_code_incremental_matches_cold(
        "ts_file_renamed",
        |root| {
            write_file(root, "package.json", r#"{"main":"src/main.ts"}"#);
            write_file(root, "src/exported.ts", "export const used = 1;\n");
            write_file(
                root,
                "src/main.ts",
                "import { used } from './exported';\nconsole.log(used);\n",
            );
        },
        |root| {
            fs::create_dir_all(root.join("src/moved")).expect("create moved dir");
            fs::rename(
                root.join("src/exported.ts"),
                root.join("src/moved/exported.ts"),
            )
            .expect("rename exported file");
        },
        &["src/moved/exported.ts"],
    );

    assert_dead_code_incremental_matches_cold(
        "tsconfig_alias_change",
        |root| {
            write_file(root, "package.json", r#"{"main":"src/main.ts"}"#);
            write_file(
                root,
                "tsconfig.json",
                r#"{"compilerOptions":{"baseUrl":".","paths":{"@lib":["src/a.ts"]}}}"#,
            );
            write_file(root, "src/a.ts", "export const x = 'a';\n");
            write_file(root, "src/b.ts", "export const x = 'b';\n");
            write_file(
                root,
                "src/main.ts",
                "import { x } from '@lib';\nexport function main() { console.log(x); }\n",
            );
        },
        |root| {
            write_file(
                root,
                "tsconfig.json",
                r#"{"compilerOptions":{"baseUrl":".","paths":{"@lib":["src/b.ts"]}}}"#,
            );
        },
        &["tsconfig.json"],
    );

    assert_dead_code_incremental_matches_cold(
        "ts_barrel_target_changed",
        |root| {
            write_file(root, "package.json", r#"{"main":"src/barrel.ts"}"#);
            write_file(
                root,
                "src/target.ts",
                "export const named = 1;\nexport default function def() { return named; }\n",
            );
            write_file(root, "src/extra.ts", "export const star = 1;\n");
            write_file(
                root,
                "src/barrel.ts",
                "export { named } from './target';\nexport { default } from './target';\nexport * from './extra';\nexport * as ns from './target';\n",
            );
        },
        |root| {
            write_file(
                root,
                "src/target.ts",
                "export const named = 1;\nexport const added = 2;\nexport default function def() { return named + added; }\n",
            );
        },
        &["src/target.ts"],
    );

    assert_dead_code_incremental_matches_cold(
        "ts_barrel_forward_changed",
        |root| {
            write_file(root, "package.json", r#"{"main":"src/main.ts"}"#);
            write_file(
                root,
                "src/target.ts",
                "export const live = 1;\nexport const spare = 2;\n",
            );
            write_file(root, "src/barrel.ts", "export { live } from './target';\n");
            write_file(
                root,
                "src/main.ts",
                "import { live } from './barrel';\nexport function main() { return live; }\n",
            );
        },
        |root| {
            write_file(
                root,
                "src/barrel.ts",
                "export { live, spare as renamedSpare } from './target';\n",
            );
        },
        &["src/barrel.ts"],
    );

    assert_dead_code_incremental_matches_cold(
        "ts_barrel_importer_changed",
        |root| {
            write_file(root, "package.json", r#"{"main":"src/main.ts"}"#);
            write_file(
                root,
                "src/target.ts",
                "export const live = 1;\nexport const spare = 2;\n",
            );
            write_file(root, "src/barrel.ts", "export { live } from './target';\n");
            write_file(
                root,
                "src/main.ts",
                "import { live } from './barrel';\nexport function main() { return live; }\n",
            );
        },
        |root| {
            write_file(
                root,
                "src/main.ts",
                "import { live } from './target';\nexport function main() { return live; }\n",
            );
        },
        &["src/main.ts"],
    );

    assert_dead_code_incremental_matches_cold(
        "rust_last_importer_removed",
        |root| {
            write_file(
                root,
                "Cargo.toml",
                "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n",
            );
            write_file(
                root,
                "src/main.rs",
                "mod helper;\nfn main() { helper::used(); }\n",
            );
            write_file(
                root,
                "src/helper.rs",
                "pub fn used() {}\npub fn dead() {}\n",
            );
        },
        |root| {
            write_file(root, "src/main.rs", "mod helper;\nfn main() {}\n");
        },
        &["src/main.rs"],
    );

    assert_dead_code_incremental_matches_cold(
        "rust_importer_deleted",
        |root| {
            write_file(
                root,
                "Cargo.toml",
                "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n",
            );
            write_file(
                root,
                "src/main.rs",
                "mod helper;\nfn main() { helper::used(); }\n",
            );
            write_file(root, "src/helper.rs", "pub fn used() {}\n");
        },
        |root| {
            fs::remove_file(root.join("src/main.rs")).expect("delete rust importer");
        },
        &[],
    );

    assert_dead_code_incremental_matches_cold(
        "rust_pub_use_target_changed",
        |root| {
            write_file(
                root,
                "Cargo.toml",
                "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n",
            );
            write_file(root, "src/lib.rs", "pub use foo::Foo;\nmod foo;\n");
            write_file(root, "src/foo.rs", "pub struct Foo;\npub struct Dead;\n");
        },
        |root| {
            write_file(
                root,
                "src/foo.rs",
                "pub struct Foo;\npub struct Dead;\npub struct Added;\n",
            );
        },
        &["src/foo.rs"],
    );

    assert_dead_code_incremental_matches_cold(
        "rust_macro_token_refs_file_changed",
        |root| {
            write_file(
                root,
                "Cargo.toml",
                "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n",
            );
            write_file(root, "src/lib.rs", "mod ui;\n");
            write_file(
                root,
                "src/ui.rs",
                "#[no_mangle]\npub extern \"C\" fn render() { element! { Header { title() } } }\npub struct Header;\npub fn title() {}\npub fn dead() {}\n",
            );
        },
        |root| {
            write_file(
                root,
                "src/ui.rs",
                "#[no_mangle]\npub extern \"C\" fn render() { element! { Header { title() } } }\npub struct Header;\npub fn title() {}\npub fn dead() {}\npub fn added_dead() {}\n",
            );
        },
        &["src/ui.rs"],
    );

    assert_dead_code_incremental_matches_cold(
        "cross_file_type_ref_removed",
        |root| {
            write_file(
                root,
                "src/types.ts",
                "export interface Widget { id: string }\n",
            );
            write_file(
                root,
                "src/use.ts",
                "import type { Widget } from './types';\ntype Box = Widget;\n",
            );
        },
        |root| {
            write_file(root, "src/use.ts", "type Box = string;\n");
        },
        &["src/use.ts"],
    );

    assert_dead_code_incremental_matches_cold(
        "manifest_entry_point_changed",
        |root| {
            write_file(root, "package.json", r#"{"main":"src/a.ts"}"#);
            write_file(root, "src/a.ts", "export function a() {}\n");
            write_file(root, "src/b.ts", "export function b() {}\n");
        },
        |root| {
            write_file(root, "package.json", r#"{"main":"src/b.ts"}"#);
        },
        &["package.json"],
    );

    assert_dead_code_incremental_matches_cold(
        "framework_route_file_changed",
        |root| {
            write_file(
                root,
                "package.json",
                r#"{"dependencies":{"next":"latest"}}"#,
            );
            write_file(
                root,
                "app/api/route.ts",
                "export function GET() { return 1; }\nexport function privateHelper() { return 0; }\n",
            );
        },
        |root| {
            write_file(
                root,
                "app/api/route.ts",
                "export function GET() { return 2; }\nexport function privateHelper() { return 0; }\nexport function newDeadHelper() { return 0; }\n",
            );
        },
        &["app/api/route.ts"],
    );

    assert_dead_code_incremental_matches_cold(
        "framework_route_manifest_changed",
        |root| {
            write_file(
                root,
                "package.json",
                r#"{"devDependencies":{"next":"latest"},"scripts":{"docs":"vitepress dev"}}"#,
            );
            write_file(
                root,
                "app/api/route.ts",
                "export function GET() { return 1; }\nexport function privateHelper() { return 0; }\n",
            );
        },
        |root| {
            write_file(
                root,
                "package.json",
                r#"{"devDependencies":{"next":"latest"},"scripts":{"build":"next build"}}"#,
            );
        },
        &["package.json"],
    );

    assert_dead_code_incremental_matches_cold(
        "nestjs_decorator_file_changed",
        |root| {
            write_file(
                root,
                "package.json",
                r#"{"dependencies":{"@nestjs/common":"latest"}}"#,
            );
            write_file(
                root,
                "src/app.controller.ts",
                "import { Controller } from '@nestjs/common';

@Controller()
export class AppController {}
export function privateHelper() { return 0; }
",
            );
        },
        |root| {
            write_file(
                root,
                "src/app.controller.ts",
                "import { Controller } from '@nestjs/common';

@Controller()
export class AppController { list() { return 1; } }
export function privateHelper() { return 0; }
export function newDeadHelper() { return 0; }
",
            );
        },
        &["src/app.controller.ts"],
    );

    assert_dead_code_incremental_matches_cold(
        "nestjs_decorator_manifest_changed",
        |root| {
            write_file(
                root,
                "package.json",
                r#"{"devDependencies":{"@nestjs/common":"latest"},"scripts":{"docs":"vitepress dev"}}"#,
            );
            write_file(
                root,
                "src/app.controller.ts",
                "import { Controller } from '@nestjs/common';

@Controller()
export class AppController {}
export function privateHelper() { return 0; }
",
            );
        },
        |root| {
            write_file(
                root,
                "package.json",
                r#"{"devDependencies":{"@nestjs/common":"latest"},"scripts":{"start":"nest start"}}"#,
            );
        },
        &["package.json"],
    );
}

#[test]
fn inspect_dead_code_twice_cold_is_deterministic() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let root = temp_dir.path().join("dead-code-twice-cold");
    fs::create_dir_all(&root).expect("create project");
    write_file(root.as_path(), "package.json", r#"{"main":"src/main.ts"}"#);
    write_file(
        root.as_path(),
        "src/main.ts",
        "import { live } from './live';\nexport function main() { live(); }\n",
    );
    write_file(
        root.as_path(),
        "src/live.ts",
        "export function live() {}\nexport function unused() {}\n",
    );

    let inspect_a = temp_dir.path().join("dead-cold-a/inspect");
    rebuild_dead_code_callgraph_store(&root, &inspect_a);
    let manager_a = InspectManager::new();
    let (cold_a, _elapsed_a) = run_reuse_category(
        &manager_a,
        snapshot(&root, &inspect_a),
        InspectCategory::DeadCode,
    );

    let inspect_b = temp_dir.path().join("dead-cold-b/inspect");
    rebuild_dead_code_callgraph_store(&root, &inspect_b);
    let manager_b = InspectManager::new();
    let (cold_b, _elapsed_b) = run_reuse_category(
        &manager_b,
        snapshot(&root, &inspect_b),
        InspectCategory::DeadCode,
    );

    assert_eq!(cold_a.aggregate, cold_b.aggregate);
    assert_eq!(
        contribution_payloads(&root, &cold_a),
        contribution_payloads(&root, &cold_b)
    );
    assert_eq!(
        aggregate_item_symbols(&cold_a),
        aggregate_item_symbols(&cold_b)
    );
}

#[test]
fn inspect_dead_code_rust_macro_token_refs_twice_cold_is_deterministic() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let root = temp_dir.path().join("dead-code-rust-macro-twice-cold");
    fs::create_dir_all(&root).expect("create project");
    write_file(
        root.as_path(),
        "Cargo.toml",
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n",
    );
    write_file(root.as_path(), "src/lib.rs", "mod ui;\n");
    write_file(
        root.as_path(),
        "src/ui.rs",
        "#[no_mangle]\npub extern \"C\" fn render() { element! { Header { title() } } }\npub struct Header;\npub fn title() {}\npub fn dead() {}\n",
    );

    let inspect_a = temp_dir.path().join("rust-macro-cold-a/inspect");
    rebuild_dead_code_callgraph_store(&root, &inspect_a);
    let manager_a = InspectManager::new();
    let (cold_a, _elapsed_a) = run_reuse_category(
        &manager_a,
        snapshot(&root, &inspect_a),
        InspectCategory::DeadCode,
    );

    let inspect_b = temp_dir.path().join("rust-macro-cold-b/inspect");
    rebuild_dead_code_callgraph_store(&root, &inspect_b);
    let manager_b = InspectManager::new();
    let (cold_b, _elapsed_b) = run_reuse_category(
        &manager_b,
        snapshot(&root, &inspect_b),
        InspectCategory::DeadCode,
    );

    assert_eq!(cold_a.aggregate, cold_b.aggregate);
    assert_eq!(
        contribution_payloads(&root, &cold_a),
        contribution_payloads(&root, &cold_b)
    );
    assert_eq!(
        aggregate_item_symbols(&cold_a),
        aggregate_item_symbols(&cold_b)
    );
    eprintln!(
        "aggregate={}",
        serde_json::to_string_pretty(&cold_a.aggregate).unwrap()
    );
    eprintln!(
        "payloads={}",
        serde_json::to_string_pretty(&contribution_payloads(&root, &cold_a)).unwrap()
    );
    assert!(!aggregate_contains_symbol(&cold_a, "src/ui.rs", "Header"));
    assert!(!aggregate_contains_symbol(&cold_a, "src/ui.rs", "title"));
    assert!(aggregate_contains_symbol(&cold_a, "src/ui.rs", "dead"));
}

#[test]
fn inspect_dead_code_framework_routes_twice_cold_is_deterministic() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let root = temp_dir.path().join("dead-code-route-twice-cold");
    fs::create_dir_all(&root).expect("create project");
    write_file(
        &root,
        "package.json",
        r#"{"dependencies":{"next":"latest"}}"#,
    );
    write_file(
        &root,
        "app/api/route.ts",
        "import { live } from './service';\nexport function GET() { live(); }\nexport function privateHelper() {}\n",
    );
    write_file(
        &root,
        "app/api/service.ts",
        "export function live() {}\nexport function unused() {}\n",
    );

    let inspect_a = temp_dir.path().join("route-cold-a/inspect");
    rebuild_dead_code_callgraph_store(&root, &inspect_a);
    let manager_a = InspectManager::new();
    let (cold_a, _elapsed_a) = run_reuse_category(
        &manager_a,
        snapshot(&root, &inspect_a),
        InspectCategory::DeadCode,
    );

    let inspect_b = temp_dir.path().join("route-cold-b/inspect");
    rebuild_dead_code_callgraph_store(&root, &inspect_b);
    let manager_b = InspectManager::new();
    let (cold_b, _elapsed_b) = run_reuse_category(
        &manager_b,
        snapshot(&root, &inspect_b),
        InspectCategory::DeadCode,
    );

    assert_eq!(cold_a.aggregate, cold_b.aggregate);
    assert_eq!(
        contribution_payloads(&root, &cold_a),
        contribution_payloads(&root, &cold_b)
    );
    assert_eq!(
        aggregate_item_symbols(&cold_a),
        aggregate_item_symbols(&cold_b)
    );
}

#[test]
fn inspect_dead_code_nestjs_decorators_twice_cold_is_deterministic() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let root = temp_dir.path().join("dead-code-nestjs-twice-cold");
    fs::create_dir_all(&root).expect("create project");
    write_file(
        &root,
        "package.json",
        r#"{"dependencies":{"@nestjs/common":"latest"}}"#,
    );
    write_file(
        &root,
        "src/app.controller.ts",
        "import { Controller } from '@nestjs/common';
import { live } from './service';

@Controller()
export class AppController { list() { return live(); } }
export function privateHelper() {}
",
    );
    write_file(
        &root,
        "src/service.ts",
        "export function live() { return 1; }\nexport function unused() { return 0; }\n",
    );

    let inspect_a = temp_dir.path().join("nestjs-cold-a/inspect");
    rebuild_dead_code_callgraph_store(&root, &inspect_a);
    let manager_a = InspectManager::new();
    let (cold_a, _elapsed_a) = run_reuse_category(
        &manager_a,
        snapshot(&root, &inspect_a),
        InspectCategory::DeadCode,
    );

    let inspect_b = temp_dir.path().join("nestjs-cold-b/inspect");
    rebuild_dead_code_callgraph_store(&root, &inspect_b);
    let manager_b = InspectManager::new();
    let (cold_b, _elapsed_b) = run_reuse_category(
        &manager_b,
        snapshot(&root, &inspect_b),
        InspectCategory::DeadCode,
    );

    assert_eq!(cold_a.aggregate, cold_b.aggregate);
    assert_eq!(
        contribution_payloads(&root, &cold_a),
        contribution_payloads(&root, &cold_b)
    );
    assert_eq!(
        aggregate_item_symbols(&cold_a),
        aggregate_item_symbols(&cold_b)
    );
}

#[test]
fn inspect_unused_exports_incremental_oxc_invariants_match_cold() {
    assert_unused_exports_incremental_matches_cold(
        "test_only_import_removed",
        |root| {
            write_file(
                root,
                "src/api.ts",
                "export function testOnly() {}
export function plantedDead() {}
",
            );
            write_file(
                root,
                "src/api.test.ts",
                "import { testOnly } from './api';
testOnly();
",
            );
        },
        |root| {
            write_file(
                root,
                "src/api.test.ts",
                "console.log('import removed');
",
            );
        },
    );

    assert_unused_exports_incremental_matches_cold(
        "last_importer_removed",
        |root| {
            write_file(
                root,
                "src/exported.ts",
                "export const x = 1;
export const y = 2;
",
            );
            write_file(
                root,
                "src/use.ts",
                "import { x } from './exported';
console.log(x);
",
            );
        },
        |root| {
            write_file(
                root,
                "src/use.ts",
                "console.log('import removed');
",
            );
        },
    );

    assert_unused_exports_incremental_matches_cold(
        "importer_deleted",
        |root| {
            write_file(
                root,
                "src/exported.ts",
                "export const x = 1;
",
            );
            write_file(
                root,
                "src/use.ts",
                "import { x } from './exported';
console.log(x);
",
            );
        },
        |root| {
            fs::remove_file(root.join("src/use.ts")).expect("delete importer");
        },
    );

    assert_unused_exports_incremental_matches_cold(
        "file_renamed",
        |root| {
            write_file(
                root,
                "src/exported.ts",
                "export const x = 1;
",
            );
            write_file(
                root,
                "src/use.ts",
                "import { x } from './exported';
console.log(x);
",
            );
        },
        |root| {
            fs::create_dir_all(root.join("src/moved")).expect("create moved dir");
            fs::rename(
                root.join("src/exported.ts"),
                root.join("src/moved/exported.ts"),
            )
            .expect("rename exported file");
        },
    );

    assert_unused_exports_incremental_matches_cold(
        "tsconfig_alias_change",
        |root| {
            write_file(
                root,
                "tsconfig.json",
                r#"{"compilerOptions":{"baseUrl":".","paths":{"@lib":["src/a.ts"]}}}"#,
            );
            write_file(
                root,
                "src/a.ts",
                "export const x = 'a';
",
            );
            write_file(
                root,
                "src/b.ts",
                "export const x = 'b';
",
            );
            write_file(
                root,
                "src/use.ts",
                "import { x } from '@lib';
console.log(x);
",
            );
        },
        |root| {
            write_file(
                root,
                "tsconfig.json",
                r#"{"compilerOptions":{"baseUrl":".","paths":{"@lib":["src/b.ts"]}}}"#,
            );
        },
    );

    assert_unused_exports_incremental_matches_cold(
        "tsconfig_base_alias_change",
        |root| {
            write_file(
                root,
                "tsconfig.json",
                r#"{"extends":"./tsconfig.base.json"}"#,
            );
            write_file(
                root,
                "tsconfig.base.json",
                r#"{"compilerOptions":{"baseUrl":".","paths":{"@lib":["src/a.ts"]}}}"#,
            );
            write_file(
                root,
                "src/a.ts",
                "export const x = 'a';
",
            );
            write_file(
                root,
                "src/b.ts",
                "export const x = 'b';
",
            );
            write_file(
                root,
                "src/use.ts",
                "import { x } from '@lib';
console.log(x);
",
            );
        },
        |root| {
            write_file(
                root,
                "tsconfig.base.json",
                r#"{"compilerOptions":{"baseUrl":".","paths":{"@lib":["src/b.ts"]}}}"#,
            );
        },
    );

    assert_unused_exports_incremental_matches_cold(
        "barrel_target_changed",
        |root| {
            write_file(
                root,
                "src/target.ts",
                "export const named = 1;
export default function def() { return named; }
",
            );
            write_file(
                root,
                "src/extra.ts",
                "export const star = 1;
",
            );
            write_file(
                root,
                "src/barrel.ts",
                "export { named } from './target';
export { default } from './target';
export * from './extra';
export * as ns from './target';
",
            );
            write_file(
                root,
                "src/use.ts",
                "import { named, default as def, star, ns } from './barrel';
console.log(named, def, star, ns);
",
            );
        },
        |root| {
            write_file(
                root,
                "src/target.ts",
                "export const named = 1;
export const added = 2;
export default function def() { return named + added; }
",
            );
        },
    );

    assert_unused_exports_incremental_matches_cold(
        "barrel_forward_changed",
        |root| {
            write_file(
                root,
                "src/target.ts",
                "export const live = 1;\nexport const spare = 2;\n",
            );
            write_file(root, "src/barrel.ts", "export { live } from './target';\n");
            write_file(
                root,
                "src/use.ts",
                "import { live } from './barrel';\nconsole.log(live);\n",
            );
        },
        |root| {
            write_file(
                root,
                "src/barrel.ts",
                "export { live, spare as renamedSpare } from './target';\n",
            );
        },
    );

    assert_unused_exports_incremental_matches_cold(
        "barrel_importer_changed",
        |root| {
            write_file(
                root,
                "src/target.ts",
                "export const live = 1;\nexport const spare = 2;\n",
            );
            write_file(root, "src/barrel.ts", "export { live } from './target';\n");
            write_file(
                root,
                "src/use.ts",
                "import { live } from './barrel';\nconsole.log(live);\n",
            );
        },
        |root| {
            write_file(
                root,
                "src/use.ts",
                "import { live } from './target';\nconsole.log(live);\n",
            );
        },
    );

    assert_unused_exports_incremental_matches_cold(
        "namespace_import_uncertain",
        |root| {
            write_file(
                root,
                "src/target.ts",
                "export const a = 1;
export const b = 2;
",
            );
            write_file(
                root,
                "src/use.ts",
                "import { a } from './target';
console.log(a);
",
            );
        },
        |root| {
            write_file(
                root,
                "src/use.ts",
                "import * as target from './target';
console.log(target);
",
            );
        },
    );

    assert_unused_exports_incremental_matches_cold(
        "dynamic_import_added",
        |root| {
            write_file(
                root,
                "src/lazy.ts",
                "export const lazy = 1;
",
            );
            write_file(
                root,
                "src/main.ts",
                "console.log('main');
",
            );
        },
        |root| {
            write_file(
                root,
                "src/main.ts",
                "import('./lazy').then((module) => console.log(module));
",
            );
        },
    );

    assert_unused_exports_incremental_matches_cold(
        "dynamic_import_removed",
        |root| {
            write_file(
                root,
                "src/lazy.ts",
                "export const lazy = 1;
",
            );
            write_file(
                root,
                "src/main.ts",
                "import('./lazy').then((module) => console.log(module));
",
            );
        },
        |root| {
            write_file(
                root,
                "src/main.ts",
                "console.log('main');
",
            );
        },
    );

    assert_unused_exports_incremental_matches_cold(
        "new_sibling_resolution_candidate",
        |root| {
            write_file(
                root,
                "src/foo/index.ts",
                "export const x = 1;
export const oldOnly = 2;
",
            );
            write_file(
                root,
                "src/use.ts",
                "import { x } from './foo';
console.log(x);
",
            );
        },
        |root| {
            write_file(
                root,
                "src/foo.ts",
                "export const x = 1;
export const newOnly = 3;
",
            );
        },
    );

    assert_unused_exports_incremental_matches_cold(
        "nestjs_decorator_file_changed",
        |root| {
            write_file(
                root,
                "package.json",
                r#"{"dependencies":{"@nestjs/common":"latest"}}"#,
            );
            write_file(
                root,
                "src/app.controller.ts",
                "import { Controller } from '@nestjs/common';

@Controller()
export class AppController {}
export function privateHelper() { return 0; }
",
            );
        },
        |root| {
            write_file(
                root,
                "src/app.controller.ts",
                "import { Controller } from '@nestjs/common';

@Controller()
export class AppController { list() { return 1; } }
export function privateHelper() { return 0; }
export function newUnusedHelper() { return 0; }
",
            );
        },
    );

    assert_unused_exports_incremental_matches_cold(
        "nestjs_decorator_manifest_changed",
        |root| {
            write_file(
                root,
                "package.json",
                r#"{"devDependencies":{"@nestjs/common":"latest"},"scripts":{"docs":"vitepress dev"}}"#,
            );
            write_file(
                root,
                "src/app.controller.ts",
                "import { Controller } from '@nestjs/common';

@Controller()
export class AppController {}
export function privateHelper() { return 0; }
",
            );
        },
        |root| {
            write_file(
                root,
                "package.json",
                r#"{"devDependencies":{"@nestjs/common":"latest"},"scripts":{"start":"nest start"}}"#,
            );
        },
    );
}

#[cfg(unix)]
#[test]
fn inspect_unused_exports_oxc_read_error_surfaces_after_cached_rollup() {
    use std::os::unix::fs::PermissionsExt;

    fn assert_read_error(success: &InspectScanSuccess, relative_file: &str) {
        assert_eq!(
            success.aggregate["complete"].as_bool(),
            Some(false),
            "read error should make aggregate incomplete: {:#}",
            success.aggregate
        );
        let parse_errors = success.aggregate["parse_errors"]
            .as_array()
            .expect("parse_errors array");
        assert!(
            parse_errors.iter().any(|error| {
                error.get("file").and_then(Value::as_str) == Some(relative_file)
                    && error
                        .get("message")
                        .and_then(Value::as_str)
                        .is_some_and(|message| message.contains("read:"))
            }),
            "expected read error for {relative_file}: {:#}",
            success.aggregate
        );
    }

    let temp_dir = tempfile::tempdir().expect("tempdir");
    let root = temp_dir.path().join("project-unreadable-unused-exports");
    fs::create_dir_all(&root).expect("create project");
    write_file(&root, "package.json", r#"{}"#);
    write_file(
        &root,
        "src/good.ts",
        "export const good = 1;
",
    );
    let unreadable = write_file(
        &root,
        "src/unreadable.ts",
        "export const hidden = 1;
",
    );
    fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000))
        .expect("make fixture unreadable");

    let inspect_dir = temp_dir.path().join("inspect-unreadable");
    let manager = InspectManager::new();
    let (first, _first_elapsed) = run_reuse_category(
        &manager,
        snapshot(&root, &inspect_dir),
        InspectCategory::UnusedExports,
    );
    if first.aggregate["complete"].as_bool() != Some(false) {
        fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o644))
            .expect("restore readable fixture");
        eprintln!("skipping unreadable-file assertion because this process can read chmod 000");
        return;
    }
    assert_read_error(&first, "src/unreadable.ts");

    write_file(&root, "package.json", r#"{"main":"src/good.ts"}"#);
    let (second, _second_elapsed) = run_reuse_category(
        &manager,
        snapshot(&root, &inspect_dir),
        InspectCategory::UnusedExports,
    );

    let second_scanned = relative_paths(&root, &second.scanned_files);
    assert!(
        !second_scanned.iter().any(|path| path == "src/good.ts"),
        "error-only contributions should not force a full JS/TS facts refresh: {second_scanned:?}"
    );
    assert_read_error(&second, "src/unreadable.ts");
    assert!(
        second.contributions.iter().any(|contribution| {
            contribution.contribution["file"].as_str() == Some("src/unreadable.ts")
                && contribution.contribution["parse_errors"]
                    .as_array()
                    .is_some_and(|errors| !errors.is_empty())
        }),
        "cached roll-up should load an error contribution for unreadable.ts"
    );

    fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o644))
        .expect("restore readable fixture");
}

#[test]
fn inspect_unused_exports_twice_cold_is_deterministic() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let root = temp_dir.path().join("project-twice-cold");
    fs::create_dir_all(&root).expect("create project");
    write_file(
        root.as_path(),
        "src/a.ts",
        "export const a = 1;
export const unused = 2;
",
    );
    write_file(
        root.as_path(),
        "src/b.ts",
        "import { a } from './a';
console.log(a);
",
    );

    let manager_a = InspectManager::new();
    let (cold_a, _elapsed_a) = run_reuse_category(
        &manager_a,
        snapshot(&root, &temp_dir.path().join("inspect-cold-a")),
        InspectCategory::UnusedExports,
    );
    let manager_b = InspectManager::new();
    let (cold_b, _elapsed_b) = run_reuse_category(
        &manager_b,
        snapshot(&root, &temp_dir.path().join("inspect-cold-b")),
        InspectCategory::UnusedExports,
    );

    assert_eq!(cold_a.aggregate, cold_b.aggregate);
    assert_eq!(
        unused_contribution_payloads(&root, &cold_a),
        unused_contribution_payloads(&root, &cold_b)
    );
}
