use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use aft::commands::configure::handle_configure;
use aft::config::Config;
use aft::context::AppContext;
use aft::lsp::registry::ServerKind;
use aft::parser::TreeSitterProvider;
use aft::protocol::RawRequest;
use serde_json::{json, Value};

fn fixture_project() -> (tempfile::TempDir, PathBuf) {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let root = temp_dir.path().join("project");
    fs::create_dir_all(&root).expect("create project root");
    (temp_dir, root)
}

fn fake_server_path() -> PathBuf {
    std::env::var_os("NEXTEST_BIN_EXE_fake_lsp_server")
        .or_else(|| std::env::var_os("NEXTEST_BIN_EXE_fake-lsp-server"))
        .map(PathBuf::from)
        .or_else(|| {
            option_env!("CARGO_BIN_EXE_fake-lsp-server")
                .or(option_env!("CARGO_BIN_EXE_fake_lsp_server"))
                .map(PathBuf::from)
        })
        .or_else(|| std::env::var_os("CARGO_BIN_EXE_fake-lsp-server").map(PathBuf::from))
        .or_else(|| std::env::var_os("CARGO_BIN_EXE_fake_lsp_server").map(PathBuf::from))
        .or_else(|| {
            let mut path = std::env::current_exe().ok()?;
            path.pop();
            path.pop();
            path.push("fake-lsp-server");
            Some(path)
        })
        .filter(|path| path.exists())
        .expect("fake-lsp-server binary path not set")
}

fn write_file(root: &Path, relative_path: &str, contents: &str) -> PathBuf {
    let path = root.join(relative_path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create fixture parent");
    }
    fs::write(&path, contents).expect("write fixture file");
    path
}

fn request(payload: Value) -> RawRequest {
    serde_json::from_value(payload).expect("request parses")
}

fn configured_context(root: &Path) -> AppContext {
    crate::helpers::disable_in_process_file_watcher();
    fs::write(root.join(".aftignore"), "**/tsconfig*.json\npackage.json\n")
        .expect("ignore config-only diagnostics files");
    let storage_dir = root.join(".aft-test-storage");
    let ctx = AppContext::new(
        Box::new(TreeSitterProvider::new()),
        Config {
            storage_dir: Some(storage_dir.clone()),
            ..Config::default()
        },
    );
    let configure = request(json!({
        "id": "configure",
        "command": "configure",
        "harness": "opencode",
        "project_root": root.to_string_lossy(),
        "storage_dir": storage_dir.to_string_lossy(),
        "config": crate::helpers::user_config(serde_json::json!({
            "search_index": false,
            "semantic_search": false
        })),
    }));
    let response = serde_json::to_value(handle_configure(&configure, &ctx))
        .expect("configure response serializes");
    assert_eq!(response["success"], true, "configure failed: {response:#}");
    ctx
}

fn configure_fake_typescript_lsp(ctx: &AppContext) {
    ctx.lsp()
        .override_binary(ServerKind::TypeScript, fake_server_path());
    ctx.lsp().set_extra_env("AFT_FAKE_LSP_PULL", "1");
}

fn inspect(ctx: &AppContext, payload: Value) -> Value {
    crate::helpers::inspect_reasking_tier1_deadline(ctx, payload)
}

fn diagnostics_details(response: &Value) -> &[Value] {
    response["details"]["diagnostics"]
        .as_array()
        .expect("diagnostics details")
}

fn inspect_diagnostics_scope(ctx: &AppContext, scope: &str) -> Value {
    let root = ctx
        .config()
        .project_root
        .clone()
        .expect("configured project root");
    let file = root.join(scope);
    let content = fs::read_to_string(&file).expect("read scoped diagnostic fixture");
    warm_scope_lsp(ctx, &file, &content);

    inspect(
        ctx,
        json!({
            "id": format!("inspect-diagnostics-{scope}"),
            "command": "inspect",
            "sections": ["diagnostics"],
            "scope": scope,
            "topK": 20,
        }),
    )
}

fn warm_scope_lsp(ctx: &AppContext, file: &Path, content: &str) {
    open_with_lsp(ctx, file, content);
    let config = ctx.config().clone();
    let results = ctx
        .lsp()
        .pull_file_diagnostics(file, &config)
        .expect("pull scoped diagnostics");
    assert!(
        results.iter().any(|result| matches!(
            &result.outcome,
            aft::lsp::manager::PullFileOutcome::Full { .. }
        )),
        "fake LSP should complete a pull for {file:?}: {results:?}"
    );
}

fn open_with_lsp(ctx: &AppContext, file: &Path, content: &str) {
    let config = ctx.config().clone();
    ctx.lsp()
        .notify_file_changed(file, content, &config)
        .expect("notify file changed");
    let diagnostics = ctx
        .lsp()
        .wait_for_diagnostics(file, &config, Duration::from_secs(2));
    assert!(
        !diagnostics.is_empty(),
        "fake LSP should publish diagnostics for {file:?}"
    );
}

#[test]
fn inspect_diagnostics_scoped_skips_tsconfig_excluded_test_file() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "packages/pkg/tsconfig.json",
        r#"{
          // JSONC is accepted by tsconfig and must be accepted by the filter.
          "compilerOptions": {
            "types": ["bun"],
          },
          "include": ["src/**/*.ts"],
          "exclude": ["src/**/*.test.ts", "src/**/__tests__/**"],
        }"#,
    );
    write_file(
        &root,
        "packages/pkg/src/foo.test.ts",
        "import { test } from 'bun:test';\ntest('works', () => import.meta.dir);\n",
    );
    let ctx = configured_context(&root);
    configure_fake_typescript_lsp(&ctx);

    let response = inspect_diagnostics_scope(&ctx, "packages/pkg/src/foo.test.ts");

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    assert_eq!(response["summary"]["diagnostics"]["errors"], 0);
    assert!(
        diagnostics_details(&response).is_empty(),
        "excluded test diagnostics must not surface: {response:#}"
    );
}

#[test]
fn inspect_diagnostics_scoped_surfaces_included_ts_file() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "packages/pkg/tsconfig.json",
        r#"{
          "include": ["src/**/*.ts"],
          "exclude": ["src/**/*.test.ts"]
        }"#,
    );
    write_file(&root, "packages/pkg/src/foo.ts", "export const foo = 1;\n");
    let ctx = configured_context(&root);
    configure_fake_typescript_lsp(&ctx);

    let response = inspect_diagnostics_scope(&ctx, "packages/pkg/src/foo.ts");

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    assert_eq!(response["summary"]["diagnostics"]["errors"], 1);
    let details = diagnostics_details(&response);
    assert_eq!(
        details.len(),
        1,
        "included file diagnostics should surface: {response:#}"
    );
    assert_eq!(details[0]["file"], "packages/pkg/src/foo.ts");
    assert_eq!(details[0]["message"], "test pull diagnostic");
}

/// A bare count ("1 error") is not actionable — diagnostics detail must surface
/// WITHOUT an explicit `sections` request (the always-on diagnostics-detail
/// behavior). Other categories stay sections-gated; diagnostics is the
/// exception because it replaced the removed `lsp_diagnostics` tool.
#[test]
fn inspect_diagnostics_detail_surfaces_without_sections_param() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "packages/pkg/tsconfig.json",
        r#"{
          "include": ["src/**/*.ts"],
          "exclude": ["src/**/*.test.ts"]
        }"#,
    );
    write_file(&root, "packages/pkg/src/foo.ts", "export const foo = 1;\n");
    let ctx = configured_context(&root);
    configure_fake_typescript_lsp(&ctx);

    // Scope provided, but NO `sections` — the exact call shape that previously
    // returned a count with no message.
    let file = root.join("packages/pkg/src/foo.ts");
    warm_scope_lsp(&ctx, &file, "export const foo = 1;\n");
    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-diagnostics-no-sections",
            "command": "inspect",
            "scope": "packages/pkg/src/foo.ts",
            "topK": 20,
        }),
    );

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    assert_eq!(response["summary"]["diagnostics"]["errors"], 1);
    let details = diagnostics_details(&response);
    assert_eq!(
        details.len(),
        1,
        "diagnostics detail must surface without a sections request: {response:#}"
    );
    assert_eq!(details[0]["file"], "packages/pkg/src/foo.ts");
    assert_eq!(details[0]["message"], "test pull diagnostic");
}

/// Self-suppression: when there are no diagnostics, the always-on path must NOT
/// inject an empty `details.diagnostics` — the clean payload stays detail-free
/// so a green inspect costs no extra tokens. Uses a tsconfig-EXCLUDED file
/// (the server never pulls it → zero diagnostics) to reach the no-items state
/// without a fake-server knob.
#[test]
fn inspect_diagnostics_zero_items_has_no_details_without_sections() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "packages/pkg/tsconfig.json",
        r#"{
          "include": ["src/**/*.ts"],
          "exclude": ["src/**/*.test.ts"]
        }"#,
    );
    write_file(
        &root,
        "packages/pkg/src/foo.test.ts",
        "import { test } from 'bun:test';\ntest('works', () => import.meta.dir);\n",
    );
    let ctx = configured_context(&root);
    configure_fake_typescript_lsp(&ctx);

    // Excluded file, NO `sections` — zero diagnostics, so the always-on path
    // must not inject an empty diagnostics detail array.
    let file = root.join("packages/pkg/src/foo.test.ts");
    warm_scope_lsp(
        &ctx,
        &file,
        "import { test } from 'bun:test';\ntest('works', () => import.meta.dir);\n",
    );
    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-diagnostics-zero-no-sections",
            "command": "inspect",
            "scope": "packages/pkg/src/foo.test.ts",
            "topK": 20,
        }),
    );

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    assert_eq!(response["summary"]["diagnostics"]["errors"], 0);
    assert!(
        response["details"].get("diagnostics").is_none(),
        "zero-diagnostics scope must not inject empty diagnostics detail: {response:#}"
    );
}

#[test]
fn inspect_diagnostics_warm_filters_excluded_file_and_keeps_included_file() {
    let (_temp_dir, root) = fixture_project();
    let included = write_file(&root, "pkg/src/included.ts", "export const included = 1;\n");
    let excluded = write_file(
        &root,
        "pkg/src/included.test.ts",
        "import { test } from 'bun:test';\ntest('x', () => import.meta.dir);\n",
    );
    write_file(
        &root,
        "pkg/tsconfig.json",
        r#"{
          "include": ["src/**/*.ts"],
          "exclude": ["src/**/*.test.ts"]
        }"#,
    );
    let ctx = configured_context(&root);
    configure_fake_typescript_lsp(&ctx);
    open_with_lsp(&ctx, &included, "export const included = 1;\n");
    open_with_lsp(
        &ctx,
        &excluded,
        "import { test } from 'bun:test';\ntest('x', () => import.meta.dir);\n",
    );

    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-diagnostics-warm-tsconfig-membership",
            "command": "inspect",
            "sections": ["diagnostics"],
            "topK": 20,
        }),
    );

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    assert_eq!(response["summary"]["diagnostics"]["errors"], 1);
    assert_eq!(response["summary"]["diagnostics"]["warnings"], 1);
    let details = diagnostics_details(&response);
    assert_eq!(
        details.len(),
        2,
        "only included warm diagnostics should surface: {response:#}"
    );
    assert!(details
        .iter()
        .all(|item| item["file"] == "pkg/src/included.ts"));
}

#[test]
fn inspect_diagnostics_extends_chain_applies_inherited_exclude() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "packages/pkg/tsconfig.base.json",
        r#"{
          "exclude": ["src/**/*.test.ts"]
        }"#,
    );
    write_file(
        &root,
        "packages/pkg/tsconfig.json",
        r#"{
          "extends": "./tsconfig.base",
          "include": ["src/**/*.ts"]
        }"#,
    );
    write_file(
        &root,
        "packages/pkg/src/chain.test.ts",
        "import { test } from 'bun:test';\ntest('x', () => import.meta.dir);\n",
    );
    let ctx = configured_context(&root);
    configure_fake_typescript_lsp(&ctx);

    let response = inspect_diagnostics_scope(&ctx, "packages/pkg/src/chain.test.ts");

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    assert!(
        diagnostics_details(&response).is_empty(),
        "exclude inherited through extends must be applied: {response:#}"
    );
}

#[test]
fn inspect_diagnostics_bare_package_extends_keeps_membership_filter() {
    let (_temp_dir, root) = fixture_project();
    write_file(&root, "node_modules/@tsconfig/bun/tsconfig.json", "{}\n");
    write_file(
        &root,
        "pkg/tsconfig.json",
        r#"{"extends":"@tsconfig/bun/tsconfig.json","include":["src"]}"#,
    );
    write_file(&root, "pkg/scripts/b.ts", "export const b = 2;\n");
    write_file(&root, "pkg/src/a.ts", "export const a = 1;\n");
    let ctx = configured_context(&root);
    configure_fake_typescript_lsp(&ctx);

    let excluded = inspect_diagnostics_scope(&ctx, "pkg/scripts/b.ts");
    assert_eq!(excluded["success"], true, "inspect failed: {excluded:#}");
    assert!(
        diagnostics_details(&excluded).is_empty(),
        "excluded file leaked: {excluded:#}"
    );

    let included = inspect_diagnostics_scope(&ctx, "pkg/src/a.ts");
    assert_eq!(included["success"], true, "inspect failed: {included:#}");
    assert_eq!(included["summary"]["diagnostics"]["errors"], 1);
    assert_eq!(diagnostics_details(&included)[0]["file"], "pkg/src/a.ts");
}

#[test]
fn inspect_diagnostics_no_tsconfig_keeps_current_behavior() {
    let (_temp_dir, root) = fixture_project();
    write_file(&root, "package.json", "{\"name\":\"no-tsconfig\"}\n");
    write_file(&root, "src/plain.ts", "export const plain = 1;\n");
    let ctx = configured_context(&root);
    configure_fake_typescript_lsp(&ctx);

    let response = inspect_diagnostics_scope(&ctx, "src/plain.ts");

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    assert_eq!(response["summary"]["diagnostics"]["errors"], 1);
    assert_eq!(
        diagnostics_details(&response).len(),
        1,
        "no-tsconfig files must not be skipped: {response:#}"
    );
}

#[test]
fn inspect_diagnostics_malformed_tsconfig_falls_through() {
    let (_temp_dir, root) = fixture_project();
    write_file(&root, "tsconfig.json", "{ this is not valid jsonc");
    write_file(&root, "src/malformed.ts", "export const malformed = 1;\n");
    let ctx = configured_context(&root);
    configure_fake_typescript_lsp(&ctx);

    let response = inspect_diagnostics_scope(&ctx, "src/malformed.ts");

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    assert_eq!(response["summary"]["diagnostics"]["errors"], 1);
    assert_eq!(
        diagnostics_details(&response).len(),
        1,
        "malformed tsconfig should fall through instead of skipping: {response:#}"
    );
}

// ---------------------------------------------------------------------------
// Status-bar E/W parity (v0.35): the agent status bar must agree with
// `tsc`/`aft_inspect`, not raw LSP. These exercise the real
// `AppContext::status_bar_counts()` path end to end.
// ---------------------------------------------------------------------------

#[test]
fn status_bar_counts_filter_build_excluded_files() {
    let (_temp_dir, root) = fixture_project();
    // tsconfig.json includes only `.ts` under src/ and excludes test files.
    // The `.tsx` is NOT in `include` and the `.test.ts` is excluded: both are
    // out-of-build, exactly the magic-context shape that produced the false E55.
    write_file(
        &root,
        "pkg/tsconfig.json",
        r#"{
          "include": ["src/**/*.ts"],
          "exclude": ["src/**/*.test.ts"]
        }"#,
    );
    let included = write_file(&root, "pkg/src/app.ts", "export const app = 1;\n");
    let excluded_test = write_file(
        &root,
        "pkg/src/app.test.ts",
        "import { test } from 'bun:test';\ntest('x', () => import.meta.dir);\n",
    );
    let excluded_tsx = write_file(&root, "pkg/src/widget.tsx", "export const w = 1;\n");
    let ctx = configured_context(&root);
    configure_fake_typescript_lsp(&ctx);

    // Each opened file gets one fake error published into the warm set.
    open_with_lsp(&ctx, &included, "export const app = 1;\n");
    open_with_lsp(&ctx, &excluded_test, "export const t = 1;\n");
    open_with_lsp(&ctx, &excluded_tsx, "export const w = 1;\n");

    // Seed Tier-2 so the bar surfaces at all (it is gated on Tier-2 presence).
    ctx.update_status_bar_tier2(Some(0), Some(0), Some(0), Some(0), false);

    let counts = ctx
        .status_bar_counts()
        .expect("status bar surfaces once Tier-2 is seeded");

    // The fake LSP publishes 1 error + 1 warning per opened file on didOpen, so
    // the raw union here is 3 errors + 3 warnings across the three files. Only
    // the in-build `app.ts` survives filtering: bar agrees with tsc/aft_inspect.
    assert_eq!(
        counts.errors, 1,
        "status bar must filter build-excluded .test.ts and .tsx (raw would be 3)"
    );
    assert_eq!(
        counts.warnings, 1,
        "only the in-build file's warning survives (raw would be 3)"
    );
}

#[test]
fn status_bar_counts_invalidate_on_tsconfig_clear() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "pkg/tsconfig.json",
        r#"{
          "include": ["src/**/*.ts"],
          "exclude": ["src/**/*.test.ts"]
        }"#,
    );
    let excluded_test = write_file(
        &root,
        "pkg/src/app.test.ts",
        "import { test } from 'bun:test';\ntest('x', () => import.meta.dir);\n",
    );
    let ctx = configured_context(&root);
    configure_fake_typescript_lsp(&ctx);
    open_with_lsp(&ctx, &excluded_test, "export const t = 1;\n");
    ctx.update_status_bar_tier2(Some(0), Some(0), Some(0), Some(0), false);

    // Excluded file is filtered out -> 0 errors.
    assert_eq!(ctx.status_bar_counts().expect("surfaces").errors, 0);

    // Clearing the membership cache (as the watcher does on a tsconfig change)
    // must not change the verdict: a re-resolved cache reaches the same
    // build-membership decision from disk, proving invalidation re-reads
    // correctly rather than losing the filter.
    ctx.clear_tsconfig_membership_cache();
    assert_eq!(
        ctx.status_bar_counts().expect("surfaces").errors,
        0,
        "membership filter must survive cache invalidation (re-resolved from disk)"
    );
}
