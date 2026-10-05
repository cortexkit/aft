//! Integration tests for `move_symbol` through the binary protocol.
//!
//! Uses temp-dir isolation (copy fixtures, mutate copies, verify results)
//! to test the full move pipeline: symbol extraction, destination insertion,
//! consumer import rewiring, checkpoint creation/restore, and error paths.

use crate::helpers::{fixture_path, user_config, AftProcess};
use serde_json::json;

/// Copy the `tests/fixtures/move_symbol/` directory into a temp dir,
/// including the `features/` subdirectory.  Returns `(TempDir, root_path)`.
fn setup_move_fixture() -> (tempfile::TempDir, String) {
    let fixtures = fixture_path("move_symbol");
    let tmp = tempfile::tempdir().expect("create temp dir");

    // Copy top-level fixture files
    for entry in std::fs::read_dir(&fixtures).expect("read fixtures dir") {
        let entry = entry.expect("read entry");
        let src = entry.path();
        if src.is_file() {
            let dst = tmp.path().join(entry.file_name());
            std::fs::copy(&src, &dst).expect("copy fixture file");
        }
    }

    // Copy features/ subdirectory
    let features_src = fixtures.join("features");
    if features_src.is_dir() {
        let features_dst = tmp.path().join("features");
        std::fs::create_dir_all(&features_dst).expect("create features dir");
        for entry in std::fs::read_dir(&features_src).expect("read features dir") {
            let entry = entry.expect("read entry");
            let src = entry.path();
            if src.is_file() {
                let dst = features_dst.join(entry.file_name());
                std::fs::copy(&src, &dst).expect("copy feature fixture");
            }
        }
    }

    let root = tmp.path().display().to_string();
    (tmp, root)
}

/// Helper: configure aft with the given project root and assert success.
fn configure(aft: &mut AftProcess, root: &str) {
    let resp = aft.send(&format!(
        r#"{{"id":"cfg","command":"configure","harness":"opencode","project_root":{}}}"#,
        crate::helpers::json_string(&root)
    ));
    assert_eq!(
        resp["success"], true,
        "configure should succeed: {:?}",
        resp
    );
}

fn configure_with_backup_disabled(aft: &mut AftProcess, root: &str) {
    let resp = aft.send(&format!(
        r#"{{"id":"cfg","command":"configure","harness":"opencode","project_root":{},"config":[{{"tier":"user","source":"test","doc":{}}}]}}"#,
        crate::helpers::json_string(&root),
        crate::helpers::json_string(&r#"{ "backup": { "enabled": false } }"#.to_string())
    ));
    assert_eq!(
        resp["success"], true,
        "configure should succeed: {:?}",
        resp
    );
}

/// This measurement intentionally pins the existing intermediate-project
/// validation behavior. Batching checker runs needs a separate output-contract
/// decision, so it is not part of the edit algorithm optimization.
#[cfg(unix)]
#[test]
#[ignore = "requires the repository's installed TypeScript compiler"]
fn multi_file_full_validation_counts_intermediate_project_checks() {
    use std::os::unix::fs::PermissionsExt;
    let tsc = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../node_modules/typescript/bin/tsc")
        .canonicalize()
        .expect("bun install before the measurement");
    for command in ["apply_patch", "move_symbol"] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_file(
            &root.join("tsconfig.json"),
            r#"{"compilerOptions":{"strict":true,"noEmit":true,"target":"ES2020","module":"ESNext"},"include":["*.ts"]}"#,
        );
        let source =
            "export function moved(n: number): number { return n + 1; }\nexport const kept = 1;\n";
        let consumer = "import { moved } from './source';\nexport const result = moved(1);\n";
        write_file(&root.join("source.ts"), source);
        write_file(&root.join("consumer.ts"), consumer);
        write_file(&root.join("dest.ts"), "export const existing = 1;\n");
        let stub = root.join("node_modules/.bin/tsc");
        write_file(&stub, &format!(
            "#!/bin/sh\nprintf 'run\\n' >> '{0}/checker-count'\nout='{0}/checker-output-'$(wc -l < '{0}/checker-count' | tr -d ' ')\nnode '{1}' --pretty false > \"$out\" 2>&1\ncode=$?\ncat \"$out\"\nexit $code\n",
            root.display(), tsc.display()));
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut aft = AftProcess::spawn();
        let cfg = aft.send(&json!({"id":"cfg-count", "command":"configure", "harness":"opencode", "project_root":root,
            "config":user_config(json!({"format_on_edit":false, "validate_on_edit":"full", "checker":{"typescript":"tsc"}, "lsp":{"enabled":false}}))}).to_string());
        assert_eq!(cfg["success"], true, "{cfg}");
        let request = if command == "move_symbol" {
            json!({"id":"count", "command":command, "file":root.join("source.ts"), "symbol":"moved", "destination":root.join("dest.ts")})
        } else {
            let patch = format!("*** Begin Patch\n*** Update File: {0}/source.ts\n@@\n-export function moved(n: number): number {{ return n + 1; }}\n+export function renamed(n: number): number {{ return n + 1; }}\n*** Update File: {0}/consumer.ts\n@@\n-import {{ moved }} from './source';\n-export const result = moved(1);\n+import {{ renamed }} from './source';\n+export const result = renamed(1);\n*** Update File: {0}/dest.ts\n@@\n-export const existing = 1;\n+export const existing = 2;\n*** End Patch", root.display());
            json!({"id":"count", "command":command, "patch_text":patch})
        };
        let response = aft.send(&request.to_string());
        assert_eq!(response["success"], true, "{command}: {response}");
        assert_eq!(
            std::fs::read_to_string(root.join("checker-count"))
                .unwrap()
                .lines()
                .count(),
            3,
            "{command} checker runs"
        );
        let first = std::fs::read_to_string(root.join("checker-output-1")).unwrap();
        assert!(
            first.contains("has no exported member 'moved'"),
            "{command}: {first}"
        );
        assert!(std::fs::read_to_string(root.join("checker-output-3"))
            .unwrap()
            .is_empty());
        assert!(
            response.get("validation_errors").is_none(),
            "{command} currently discards the checker diagnostics"
        );
        // Measure the proposed single final-state run without changing the
        // command's production validation order or response contract.
        let final_check = std::process::Command::new(&stub)
            .current_dir(root)
            .output()
            .unwrap();
        assert!(final_check.status.success());
        assert!(final_check.stdout.is_empty());
        assert_eq!(
            std::fs::read_to_string(root.join("checker-count"))
                .unwrap()
                .lines()
                .count(),
            4
        );
        eprintln!("{command}: 3 current checker runs versus 1 clean final-state proposal run; first intermediate state reports TS2305; outer response discards validation_errors");
        assert!(aft.shutdown().success());
    }
}

fn write_file(path: &std::path::Path, content: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).expect("create parent");
    std::fs::write(path, content).expect("write file");
}

fn setup_namespace_move_case(
    consumer_content: &str,
) -> (
    tempfile::TempDir,
    std::path::PathBuf,
    std::path::PathBuf,
    std::path::PathBuf,
) {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let source = tmp.path().join("source.ts");
    let destination = tmp.path().join("dest.ts");
    let consumer = tmp.path().join("consumer.ts");
    write_file(
        &source,
        "export function moved(): string { return 'ok'; }\n\
export function stays(): string { return 'stay'; }\n",
    );
    write_file(&destination, "export const existing = true;\n");
    write_file(&consumer, consumer_content);
    (tmp, source, destination, consumer)
}

fn request_namespace_move(
    aft: &mut AftProcess,
    source: &std::path::Path,
    destination: &std::path::Path,
) -> serde_json::Value {
    aft.send(&format!(
        r#"{{"id":"namespace-move","command":"move_symbol","file":{},"symbol":"moved","destination":{}}}"#,
        crate::helpers::json_string(&source.display()),
        crate::helpers::json_string(&destination.display())
    ))
}

fn assert_move_symbol_unsupported(ext: &str, source: &str, dest: &str) {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let src = tmp.path().join(format!("source.{ext}"));
    let dst = tmp.path().join(format!("dest.{ext}"));
    write_file(&src, source);
    write_file(&dst, dest);

    let mut aft = AftProcess::spawn();
    let resp = aft.send(&format!(
        r#"{{"id":"unsupported-{ext}","command":"move_symbol","file":{},"symbol":"Foo","destination":{}}}"#,
        crate::helpers::json_string(&src.display()),
        crate::helpers::json_string(&dst.display())
    ));
    assert_eq!(resp["success"], false, "move should fail: {resp:?}");
    assert_eq!(
        resp["code"], "unsupported_language",
        "wrong error: {resp:?}"
    );
    aft.shutdown();
}

#[test]
fn move_symbol_rejects_python_source() {
    assert_move_symbol_unsupported("py", "def Foo():\n    pass\n", "\n");
}

#[test]
fn move_symbol_rejects_rust_source() {
    assert_move_symbol_unsupported("rs", "pub fn Foo() {}\n", "\n");
}

#[test]
fn move_symbol_rejects_go_source() {
    assert_move_symbol_unsupported("go", "package main\n\nfunc Foo() {}\n", "package main\n");
}

#[test]
fn move_symbol_rewrites_barrel_named_reexport() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let root = tmp.path().display().to_string();
    let src_dir = tmp.path().join("src");
    let foo = src_dir.join("foo.ts");
    let bar = src_dir.join("bar.ts");
    let index = src_dir.join("index.ts");
    write_file(&foo, "export function Foo() { return 1; }\n");
    write_file(&bar, "export const Bar = 2;\n");
    write_file(&index, "export { Foo } from './foo';\n");

    let mut aft = AftProcess::spawn();
    configure(&mut aft, &root);
    let resp = aft.send(&format!(
        r#"{{"id":"barrel","command":"move_symbol","file":{},"symbol":"Foo","destination":{}}}"#,
        crate::helpers::json_string(&foo.display()),
        crate::helpers::json_string(&bar.display())
    ));
    assert_eq!(resp["success"], true, "move should succeed: {resp:?}");
    let index_content = std::fs::read_to_string(index).expect("read index");
    assert!(
        index_content.contains("export { Foo } from './bar';"),
        "barrel should point at ./bar:\n{index_content}"
    );
    aft.shutdown();
}

/// Regression: move_symbol must not scan or rewrite imports inside build
/// artifacts. The consumer collector now uses the shared project walker, which
/// skips standard build/cache dirs (dist/build/target/...) and respects
/// .gitignore/.aftignore. A compiled `dist/` copy that imports from the source
/// must be left untouched while the real `src/` consumer is rewritten.
#[test]
fn move_symbol_skips_build_artifacts_and_gitignored_files() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let root = tmp.path().display().to_string();
    let foo = tmp.path().join("src/foo.ts");
    let bar = tmp.path().join("src/bar.ts");
    let consumer = tmp.path().join("src/consumer.ts");
    // Build artifact under dist/ (always-excluded) importing the moved symbol.
    let dist_artifact = tmp.path().join("dist/consumer.js");
    // A gitignored generated file importing the moved symbol.
    let gitignored = tmp.path().join("generated/out.ts");

    write_file(&foo, "export function Foo() { return 1; }\n");
    write_file(&bar, "export const Bar = 2;\n");
    write_file(
        &consumer,
        "import { Foo } from './foo';\nexport const x = Foo();\n",
    );
    write_file(
        &dist_artifact,
        "import { Foo } from './foo';\nexport const y = Foo();\n",
    );
    write_file(
        &gitignored,
        "import { Foo } from './foo';\nexport const z = Foo();\n",
    );
    write_file(&tmp.path().join(".gitignore"), "generated/\n");

    let mut aft = AftProcess::spawn();
    configure(&mut aft, &root);
    let resp = aft.send(&format!(
        r#"{{"id":"skip-artifacts","command":"move_symbol","file":{},"symbol":"Foo","destination":{}}}"#,
        crate::helpers::json_string(&foo.display()),
        crate::helpers::json_string(&bar.display())
    ));
    assert_eq!(resp["success"], true, "move should succeed: {resp:?}");

    // Real consumer rewritten to the new location.
    let consumer_content = std::fs::read_to_string(&consumer).expect("read consumer");
    assert!(
        consumer_content.contains("from './bar'"),
        "real consumer should be rewritten to ./bar:\n{consumer_content}"
    );
    // Build artifact and gitignored file must be byte-for-byte untouched.
    assert_eq!(
        std::fs::read_to_string(&dist_artifact).expect("read dist"),
        "import { Foo } from './foo';\nexport const y = Foo();\n",
        "dist/ build artifact must not be rewritten"
    );
    assert_eq!(
        std::fs::read_to_string(&gitignored).expect("read gitignored"),
        "import { Foo } from './foo';\nexport const z = Foo();\n",
        "gitignored file must not be rewritten"
    );
    aft.shutdown();
}

/// BLOCKER regression (R3-C): when the DESTINATION write is rolled back for
/// invalid syntax, the symbol must NOT vanish. The source has the symbol removed
/// first; if the dest write (which adds it) silently rolls back and the move
/// still reports success, the symbol is defined nowhere. Trigger: move a symbol
/// whose body carries TS type syntax into a `.js` file, where syntax validation
/// rejects it. The move must FAIL and the source must still define the symbol.
#[test]
fn move_symbol_rolled_back_dest_does_not_lose_symbol() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let root = tmp.path().display().to_string();
    let src = tmp.path().join("src.ts");
    let dest = tmp.path().join("dest.js");

    // TS source with a symbol whose body uses a type annotation (valid in .ts).
    let src_original = "export function Foo(x: number): number { return x + 1; }\n";
    write_file(&src, src_original);
    write_file(&dest, "export const existing = 1;\n");

    let mut aft = AftProcess::spawn();
    configure_with_backup_disabled(&mut aft, &root);
    let resp = aft.send(&format!(
        r#"{{"id":"rollback-dest","command":"move_symbol","file":{},"symbol":"Foo","destination":{}}}"#,
        crate::helpers::json_string(&src.display()),
        crate::helpers::json_string(&dest.display())
    ));

    // The move must fail (dest write would be invalid .js), not silently succeed.
    assert_eq!(
        resp["success"], false,
        "move into a .js dest that rejects TS type syntax must fail, not lose the symbol: {resp:?}"
    );

    // CRITICAL: the source must still define Foo — nothing was lost.
    let src_after = std::fs::read_to_string(&src).expect("read source");
    assert!(
        src_after.contains("function Foo"),
        "source must still define Foo after a rolled-back move:\n{src_after}"
    );
    // The destination must not have been left with a broken partial write.
    let dest_after = std::fs::read_to_string(&dest).expect("read dest");
    assert!(
        !dest_after.contains("function Foo"),
        "dest must not contain a half-written Foo:\n{dest_after}"
    );
    aft.shutdown();
}

/// A tracked consumer inside a HIDDEN directory (e.g. `.storybook/`) must still
/// be rewritten — `.hidden(false)` in the consumer walk. Skipping it (as the
/// shared `walk_project_files` would, with `.hidden(true)`) leaves a dangling
/// import after the move.
#[test]
fn move_symbol_rewrites_consumers_in_hidden_dirs() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let root = tmp.path().display().to_string();
    let foo = tmp.path().join("src/foo.ts");
    let bar = tmp.path().join("src/bar.ts");
    // Tracked consumer in a hidden dir (Storybook config, not gitignored).
    let hidden_consumer = tmp.path().join(".storybook/preview.ts");

    write_file(&foo, "export function Foo() { return 1; }\n");
    write_file(&bar, "export const Bar = 2;\n");
    write_file(
        &hidden_consumer,
        "import { Foo } from '../src/foo';\nexport const x = Foo();\n",
    );

    let mut aft = AftProcess::spawn();
    configure(&mut aft, &root);
    let resp = aft.send(&format!(
        r#"{{"id":"hidden-consumer","command":"move_symbol","file":{},"symbol":"Foo","destination":{}}}"#,
        crate::helpers::json_string(&foo.display()),
        crate::helpers::json_string(&bar.display())
    ));
    assert_eq!(resp["success"], true, "move should succeed: {resp:?}");

    let content = std::fs::read_to_string(&hidden_consumer).expect("read hidden consumer");
    assert!(
        content.contains("from '../src/bar'"),
        "tracked consumer in .storybook/ should be rewritten to ../src/bar:\n{content}"
    );
    aft.shutdown();
}

// ---------------------------------------------------------------------------
// Success path tests
// ---------------------------------------------------------------------------

/// Basic move: formatDate from service.ts → utils.ts.
/// Verifies symbol removed from source, added to destination with export,
/// and consumer_a imports from the new location.
#[test]
fn move_symbol_basic() {
    let (_tmp, root) = setup_move_fixture();
    let mut aft = AftProcess::spawn();
    configure(&mut aft, &root);

    let source = format!("{}/service.ts", root);
    let dest = format!("{}/utils.ts", root);

    let resp = aft.send(&format!(
        r#"{{"id":"1","command":"move_symbol","file":{},"symbol":"formatDate","destination":{}}}"#,
        crate::helpers::json_string(&source),
        crate::helpers::json_string(&dest)
    ));

    assert_eq!(
        resp["success"], true,
        "move_symbol should succeed: {:?}",
        resp
    );
    assert!(
        resp["files_modified"].as_u64().unwrap() >= 2,
        "at least source + dest should be modified"
    );
    assert!(
        resp["consumers_updated"].as_u64().unwrap() >= 1,
        "at least one consumer should be updated"
    );
    assert!(
        resp["checkpoint_name"]
            .as_str()
            .unwrap()
            .contains("formatDate"),
        "checkpoint should reference the moved symbol"
    );

    // Verify source no longer contains formatDate function
    let source_content = std::fs::read_to_string(&source).expect("read source");
    assert!(
        !source_content.contains("export function formatDate"),
        "formatDate should be removed from source"
    );
    // Other symbols should remain
    assert!(
        source_content.contains("export function parseDate"),
        "parseDate should stay in source"
    );
    assert!(
        source_content.contains("DATE_FORMAT"),
        "DATE_FORMAT should stay in source"
    );

    // Verify destination now contains formatDate
    let dest_content = std::fs::read_to_string(&dest).expect("read dest");
    assert!(
        dest_content.contains("export function formatDate"),
        "formatDate should appear in destination with export"
    );
    // Original destination content should remain
    assert!(
        dest_content.contains("export function slugify"),
        "slugify should still be in destination"
    );

    // Verify consumer_a now imports from utils instead of service
    let consumer_a =
        std::fs::read_to_string(format!("{}/consumer_a.ts", root)).expect("read consumer_a");
    assert!(
        consumer_a.contains("'./utils'") || consumer_a.contains("\"./utils\""),
        "consumer_a should import from ./utils, got:\n{}",
        consumer_a
    );
    assert!(
        !consumer_a.contains("'./service'") || consumer_a.contains("parseDate"),
        "consumer_a should no longer import formatDate from ./service"
    );

    aft.shutdown();
}

/// Disabling the persisted callgraph store still keeps TypeScript/JavaScript
/// moves safe because `move_symbol` brute-scans TS/JS consumers independently.
#[test]
fn move_symbol_configured_without_store_still_rewrites_ts_consumers() {
    let (_tmp, root) = setup_move_fixture();
    let mut aft = AftProcess::spawn();

    let configure = aft.send(
        &json!({
            "id": "cfg",
            "command": "configure",
            "harness": "opencode",
            "project_root": root,
            "config": user_config(json!({ "callgraph_store": false }))
        })
        .to_string(),
    );
    assert_eq!(
        configure["success"], true,
        "configure should succeed: {configure:?}"
    );

    let source = format!("{}/service.ts", root);
    let dest = format!("{}/utils.ts", root);

    let resp = aft.send(&format!(
        r#"{{"id":"1","command":"move_symbol","file":{},"symbol":"formatDate","destination":{}}}"#,
        crate::helpers::json_string(&source),
        crate::helpers::json_string(&dest)
    ));

    assert_eq!(
        resp["success"], true,
        "move_symbol should succeed: {resp:?}"
    );
    assert!(
        resp["consumers_updated"].as_u64().unwrap_or(0) >= 1,
        "TS/JS consumers should still be rewritten without the store: {resp:?}"
    );

    let consumer_a =
        std::fs::read_to_string(format!("{}/consumer_a.ts", root)).expect("read consumer_a");
    assert!(
        consumer_a.contains("'./utils'") || consumer_a.contains("\"./utils\""),
        "consumer_a should import from ./utils, got:\n{}",
        consumer_a
    );

    aft.shutdown();
}

#[test]
fn move_symbol_large_project_has_no_legacy_file_cap() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let root = tmp.path().display().to_string();

    write_file(
        &tmp.path().join("service.ts"),
        "export function formatDate() { return 'today'; }\n",
    );
    write_file(
        &tmp.path().join("utils.ts"),
        "export function slugify(value: string) { return value; }\n",
    );
    write_file(
        &tmp.path().join("consumer.ts"),
        "import { formatDate } from './service';\nexport const label = formatDate();\n",
    );

    for index in 0..5_050 {
        write_file(
            &tmp.path().join(format!("filler_{index}.ts")),
            &format!("export const filler_{index} = {index};\n"),
        );
    }

    let mut aft = AftProcess::spawn();
    let configure = aft.send(
        &json!({
            "id": "cfg",
            "command": "configure",
            "harness": "opencode",
            "project_root": root,
            "config": user_config(json!({ "callgraph_store": false }))
        })
        .to_string(),
    );
    assert_eq!(
        configure["success"], true,
        "configure should succeed: {configure:?}"
    );

    let source = tmp.path().join("service.ts").display().to_string();
    let dest = tmp.path().join("utils.ts").display().to_string();
    let resp = aft.send(&format!(
        r#"{{"id":"1","command":"move_symbol","file":{},"symbol":"formatDate","destination":{}}}"#,
        crate::helpers::json_string(&source),
        crate::helpers::json_string(&dest)
    ));

    assert_eq!(
        resp["success"], true,
        "move_symbol should not hit a legacy file cap: {resp:?}"
    );
    assert_ne!(
        resp.get("code"),
        Some(&json!("project_too_large")),
        "legacy project_too_large response must be gone: {resp:?}"
    );

    let consumer = std::fs::read_to_string(tmp.path().join("consumer.ts")).expect("read consumer");
    assert!(
        consumer.contains("'./utils'") || consumer.contains("\"./utils\""),
        "consumer should import from ./utils, got:\n{}",
        consumer
    );

    aft.shutdown();
}

/// Explicitly verify ALL 5+ consumer files have correct import paths after move.
#[test]
fn move_symbol_multiple_consumers() {
    let (_tmp, root) = setup_move_fixture();
    let mut aft = AftProcess::spawn();
    configure(&mut aft, &root);

    let source = format!("{}/service.ts", root);
    let dest = format!("{}/utils.ts", root);

    let resp = aft.send(&format!(
        r#"{{"id":"1","command":"move_symbol","file":{},"symbol":"formatDate","destination":{}}}"#,
        crate::helpers::json_string(&source),
        crate::helpers::json_string(&dest)
    ));

    assert_eq!(resp["success"], true, "move should succeed: {:?}", resp);

    // consumer_a.ts — same dir, imports only formatDate
    // Should: import { formatDate } from './utils'
    let ca = std::fs::read_to_string(format!("{}/consumer_a.ts", root)).unwrap();
    assert!(
        ca.contains("'./utils'") || ca.contains("\"./utils\""),
        "consumer_a should import from ./utils:\n{}",
        ca
    );
    assert!(
        ca.contains("formatDate"),
        "consumer_a should still reference formatDate"
    );

    // consumer_b.ts — imports both formatDate and parseDate
    // Should: keep parseDate from ./service, add formatDate from ./utils
    let cb = std::fs::read_to_string(format!("{}/consumer_b.ts", root)).unwrap();
    assert!(
        cb.contains("'./utils'") || cb.contains("\"./utils\""),
        "consumer_b should have import from ./utils:\n{}",
        cb
    );
    assert!(
        cb.contains("parseDate"),
        "consumer_b should still reference parseDate"
    );

    // consumer_c.ts — aliased import { formatDate as fmtDate }
    // Should: import from ./utils with alias preserved
    let cc = std::fs::read_to_string(format!("{}/consumer_c.ts", root)).unwrap();
    assert!(
        cc.contains("'./utils'") || cc.contains("\"./utils\""),
        "consumer_c should import from ./utils:\n{}",
        cc
    );

    // consumer_d.ts — imports only DATE_FORMAT (NOT formatDate)
    // Should: remain UNCHANGED
    let cd_original =
        std::fs::read_to_string(fixture_path("move_symbol").join("consumer_d.ts")).unwrap();
    let cd = std::fs::read_to_string(format!("{}/consumer_d.ts", root)).unwrap();
    assert_eq!(
        cd.trim(),
        cd_original.trim(),
        "consumer_d should be unchanged (only imports DATE_FORMAT)"
    );

    // consumer_e.ts — in features/ subdirectory, imports via '../service'
    // Should: import from '../utils'
    let ce = std::fs::read_to_string(format!("{}/features/consumer_e.ts", root)).unwrap();
    assert!(
        ce.contains("'../utils'") || ce.contains("\"../utils\""),
        "consumer_e should import from ../utils:\n{}",
        ce
    );

    // consumer_f.ts — imports only parseDate (NOT formatDate)
    // Should: remain UNCHANGED
    let cf_original =
        std::fs::read_to_string(fixture_path("move_symbol").join("consumer_f.ts")).unwrap();
    let cf = std::fs::read_to_string(format!("{}/consumer_f.ts", root)).unwrap();
    assert_eq!(
        cf.trim(),
        cf_original.trim(),
        "consumer_f should be unchanged (only imports parseDate)"
    );

    aft.shutdown();
}

/// Aliased import: `import { formatDate as fmtDate }` should preserve alias after move.
#[test]
fn move_symbol_aliased_import() {
    let (_tmp, root) = setup_move_fixture();
    let mut aft = AftProcess::spawn();
    configure(&mut aft, &root);

    let source = format!("{}/service.ts", root);
    let dest = format!("{}/utils.ts", root);

    let resp = aft.send(&format!(
        r#"{{"id":"1","command":"move_symbol","file":{},"symbol":"formatDate","destination":{}}}"#,
        crate::helpers::json_string(&source),
        crate::helpers::json_string(&dest)
    ));

    assert_eq!(resp["success"], true, "move should succeed: {:?}", resp);

    // consumer_c uses: import { formatDate as fmtDate } from './service';
    // After move, should be: import { formatDate as fmtDate } from './utils';
    let cc = std::fs::read_to_string(format!("{}/consumer_c.ts", root)).unwrap();

    assert!(
        cc.contains("fmtDate"),
        "alias 'fmtDate' should be preserved:\n{}",
        cc
    );
    assert!(
        cc.contains("formatDate as fmtDate"),
        "alias form 'formatDate as fmtDate' should be preserved:\n{}",
        cc
    );
    assert!(
        cc.contains("'./utils'") || cc.contains("\"./utils\""),
        "should import from ./utils:\n{}",
        cc
    );

    aft.shutdown();
}

#[test]
fn move_symbol_rewrites_namespace_member_consumer() {
    let (tmp, source, destination, consumer) = setup_namespace_move_case(
        "import * as service from './source';\n\
export const result = service.moved();\n",
    );
    let mut aft = AftProcess::spawn();
    configure(&mut aft, &tmp.path().display().to_string());

    let resp = request_namespace_move(&mut aft, &source, &destination);
    assert_eq!(resp["success"], true, "move should succeed: {resp:?}");
    assert_eq!(resp["consumers_updated"], 1);
    assert_eq!(
        std::fs::read_to_string(&consumer).unwrap(),
        "import { moved } from './dest';\n\
import * as service from './source';\n\
export const result = moved();\n"
    );

    aft.shutdown();
}

#[test]
fn move_symbol_rewrites_namespace_members_in_js_and_tsx_consumers() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let source = tmp.path().join("source.ts");
    let destination = tmp.path().join("dest.ts");
    let javascript_consumer = tmp.path().join("consumer.js");
    let tsx_consumer = tmp.path().join("component.tsx");
    write_file(
        &source,
        "export function moved(): string { return 'ok'; }\n\
export function stays(): string { return 'stay'; }\n",
    );
    write_file(&destination, "export const existing = true;\n");
    write_file(
        &javascript_consumer,
        "import * as service from './source';\n\
export const result = service.moved();\n",
    );
    write_file(
        &tsx_consumer,
        "import * as service from './source';\n\
export const view = <span>{service.moved()}</span>;\n",
    );
    let mut aft = AftProcess::spawn();
    configure(&mut aft, &tmp.path().display().to_string());

    let resp = request_namespace_move(&mut aft, &source, &destination);
    assert_eq!(resp["success"], true, "move should succeed: {resp:?}");
    assert_eq!(resp["consumers_updated"], 2);
    for consumer in [&javascript_consumer, &tsx_consumer] {
        let content = std::fs::read_to_string(consumer).unwrap();
        assert!(content.contains("import { moved } from './dest';"));
        assert!(content.contains("import * as service from './source';"));
        assert!(!content.contains("service.moved"));
    }

    aft.shutdown();
}

#[test]
fn move_symbol_merges_namespace_rewrite_into_existing_destination_import() {
    let (tmp, source, destination, consumer) = setup_namespace_move_case(
        "import { existing } from './dest';\n\
import * as service from './source';\n\
export const result = service.moved() + String(existing);\n",
    );
    let mut aft = AftProcess::spawn();
    configure(&mut aft, &tmp.path().display().to_string());

    let resp = request_namespace_move(&mut aft, &source, &destination);
    assert_eq!(resp["success"], true, "move should succeed: {resp:?}");
    assert_eq!(resp["consumers_updated"], 1);
    assert_eq!(
        std::fs::read_to_string(&consumer).unwrap(),
        "import { existing, moved } from './dest';\n\
import * as service from './source';\n\
export const result = moved() + String(existing);\n"
    );

    aft.shutdown();
}

#[test]
fn move_symbol_aliases_namespace_rewrite_when_local_binding_collides() {
    let (tmp, source, destination, consumer) = setup_namespace_move_case(
        "import * as service from './source';\n\
const moved = () => 'local';\n\
export const result = service.moved() + moved();\n",
    );
    let mut aft = AftProcess::spawn();
    configure(&mut aft, &tmp.path().display().to_string());

    let resp = request_namespace_move(&mut aft, &source, &destination);
    assert_eq!(resp["success"], true, "move should succeed: {resp:?}");
    assert_eq!(resp["consumers_updated"], 1);
    assert_eq!(
        std::fs::read_to_string(&consumer).unwrap(),
        "import { moved as moved_2 } from './dest';\n\
import * as service from './source';\n\
const moved = () => 'local';\n\
export const result = moved_2() + moved();\n"
    );

    aft.shutdown();
}

#[test]
fn move_symbol_rejects_computed_namespace_access_before_writing() {
    let (tmp, source, destination, consumer) = setup_namespace_move_case(
        "import * as service from './source';\n\
export const result = service[\"moved\"]();\n",
    );
    let source_before = std::fs::read_to_string(&source).unwrap();
    let destination_before = std::fs::read_to_string(&destination).unwrap();
    let consumer_before = std::fs::read_to_string(&consumer).unwrap();
    let mut aft = AftProcess::spawn();
    configure(&mut aft, &tmp.path().display().to_string());

    let resp = request_namespace_move(&mut aft, &source, &destination);
    assert_eq!(resp["success"], false, "move should reject: {resp:?}");
    assert_eq!(resp["code"], "unsupported_namespace_usage");
    assert!(
        resp["files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|file| file.as_str().unwrap().ends_with("consumer.ts")),
        "error should name the unsupported consumer: {resp:?}"
    );
    assert_eq!(std::fs::read_to_string(&source).unwrap(), source_before);
    assert_eq!(
        std::fs::read_to_string(&destination).unwrap(),
        destination_before
    );
    assert_eq!(std::fs::read_to_string(&consumer).unwrap(), consumer_before);

    aft.shutdown();
}

#[test]
fn move_symbol_rejects_namespace_binding_used_as_value() {
    let (tmp, source, destination, consumer) = setup_namespace_move_case(
        "import * as service from './source';\n\
const inspect = (value: object) => value;\n\
export const result = service.moved() + String(inspect(service));\n",
    );
    let source_before = std::fs::read_to_string(&source).unwrap();
    let destination_before = std::fs::read_to_string(&destination).unwrap();
    let consumer_before = std::fs::read_to_string(&consumer).unwrap();
    let mut aft = AftProcess::spawn();
    configure(&mut aft, &tmp.path().display().to_string());

    let resp = request_namespace_move(&mut aft, &source, &destination);
    assert_eq!(resp["success"], false, "move should reject: {resp:?}");
    assert_eq!(resp["code"], "unsupported_namespace_usage");
    assert_eq!(std::fs::read_to_string(&source).unwrap(), source_before);
    assert_eq!(
        std::fs::read_to_string(&destination).unwrap(),
        destination_before
    );
    assert_eq!(std::fs::read_to_string(&consumer).unwrap(), consumer_before);

    aft.shutdown();
}

#[test]
fn move_symbol_leaves_unrelated_namespace_consumer_untouched() {
    let consumer_before = "import * as service from './source';\n\
const other = { moved: () => 'other' };\n\
const text = 'service.moved'; // service.moved is not executable code.\n\
export const result = service.stays() + other.moved() + text;\n";
    let (tmp, source, destination, consumer) = setup_namespace_move_case(consumer_before);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, &tmp.path().display().to_string());

    let resp = request_namespace_move(&mut aft, &source, &destination);
    assert_eq!(resp["success"], true, "move should succeed: {resp:?}");
    assert_eq!(resp["consumers_updated"], 0);
    assert_eq!(std::fs::read_to_string(&consumer).unwrap(), consumer_before);

    aft.shutdown();
}

#[test]
fn move_symbol_rejects_namespace_reexport_before_writing() {
    let (tmp, source, destination, consumer) =
        setup_namespace_move_case("export * as service from './source';\n");
    let source_before = std::fs::read_to_string(&source).unwrap();
    let destination_before = std::fs::read_to_string(&destination).unwrap();
    let consumer_before = std::fs::read_to_string(&consumer).unwrap();
    let mut aft = AftProcess::spawn();
    configure(&mut aft, &tmp.path().display().to_string());

    let resp = request_namespace_move(&mut aft, &source, &destination);
    assert_eq!(resp["success"], false, "move should reject: {resp:?}");
    assert_eq!(resp["code"], "unsupported_namespace_usage");
    assert!(
        resp["files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|file| file.as_str().unwrap().ends_with("consumer.ts")),
        "error should name the unsupported consumer: {resp:?}"
    );
    assert_eq!(std::fs::read_to_string(&source).unwrap(), source_before);
    assert_eq!(
        std::fs::read_to_string(&destination).unwrap(),
        destination_before
    );
    assert_eq!(std::fs::read_to_string(&consumer).unwrap(), consumer_before);

    aft.shutdown();
}

#[test]
fn move_symbol_rewrites_source_file_when_remaining_code_uses_moved_symbol() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let root = tmp.path().display().to_string();
    let source = tmp.path().join("source.ts");
    let dest = tmp.path().join("helpers.ts");

    write_file(
        &source,
        "export function helper(): string {
  return 'ok';
}

export function useHelper(): string {
  return helper();
}
",
    );
    write_file(
        &dest,
        "export const existing = true;
",
    );

    let mut aft = AftProcess::spawn();
    configure(&mut aft, &root);

    let resp = aft.send(&format!(
        r#"{{"id":"source-consumer","command":"move_symbol","file":{},"symbol":"helper","destination":{}}}"#,
        crate::helpers::json_string(&source.display()),
        crate::helpers::json_string(&dest.display())
    ));
    assert_eq!(resp["success"], true, "move should succeed: {resp:?}");
    assert!(
        resp["consumers_updated"].as_u64().unwrap() >= 1,
        "source file should count as an updated consumer: {resp:?}"
    );

    let source_content = std::fs::read_to_string(&source).expect("read source");
    // The source file has no other imports and no formatter config, so the new
    // statement takes the same default spelling add_import uses.
    assert!(
        source_content.contains("import { helper } from \"./helpers\";"),
        "source should import the moved helper from destination:
{source_content}"
    );
    assert!(
        source_content.contains("return helper();"),
        "remaining source code should still call helper:
{source_content}"
    );
    assert!(
        !source_content.contains("export function helper"),
        "helper declaration should be removed from source:
{source_content}"
    );

    let dest_content = std::fs::read_to_string(&dest).expect("read dest");
    assert!(
        dest_content.contains("export function helper"),
        "destination should contain moved helper:
{dest_content}"
    );

    aft.shutdown();
}

#[test]
fn move_symbol_does_not_reimport_shadowed_source_references() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let root = tmp.path().display().to_string();
    let source = tmp.path().join("source.ts");
    let dest = tmp.path().join("dest.ts");
    let consumer = tmp.path().join("consumer.ts");

    write_file(
        &source,
        r#"export function logger(): string {
  return 'moved';
}

function logData(logger: unknown): unknown {
  return logger;
}

export function f(): number {
  const logger = 1;
  return logger;
}

export function destructured(input: { logger: number }): number {
  const { logger } = input;
  return logger;
}
"#,
    );
    write_file(&dest, "export const existing = true;\n");
    write_file(
        &consumer,
        r#"import { logger } from './source';

export const value = logger();
"#,
    );

    let mut aft = AftProcess::spawn();
    configure(&mut aft, &root);

    let resp = aft.send(&format!(
        r#"{{"id":"shadowed-source","command":"move_symbol","file":{},"symbol":"logger","destination":{}}}"#,
        crate::helpers::json_string(&source.display()),
        crate::helpers::json_string(&dest.display())
    ));
    assert_eq!(resp["success"], true, "move should succeed: {resp:?}");

    let source_content = std::fs::read_to_string(&source).expect("read source");
    assert!(
        !source_content.contains("from './dest'") && !source_content.contains("from \"./dest\""),
        "shadowed source references should not add an import from dest:\n{source_content}"
    );
    assert!(
        source_content.contains("function logData(logger: unknown)")
            && source_content.contains("const logger = 1")
            && source_content.contains("const { logger } = input"),
        "shadowing declarations should remain in source:\n{source_content}"
    );

    let consumer_content = std::fs::read_to_string(&consumer).expect("read consumer");
    assert!(
        consumer_content.contains("import { logger } from './dest';"),
        "other-file consumer should import logger from dest:\n{consumer_content}"
    );
    assert!(
        !consumer_content.contains("from './source'")
            && !consumer_content.contains("from \"./source\""),
        "other-file consumer should no longer import logger from source:\n{consumer_content}"
    );

    aft.shutdown();
}

#[test]
fn move_symbol_reimports_unshadowed_source_references() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let root = tmp.path().display().to_string();
    let source = tmp.path().join("source.ts");
    let dest = tmp.path().join("dest.ts");

    write_file(
        &source,
        r#"export function logger(): string {
  return 'moved';
}

export function useLogger(): string {
  return logger();
}
"#,
    );
    write_file(&dest, "export const existing = true;\n");

    let mut aft = AftProcess::spawn();
    configure(&mut aft, &root);

    let resp = aft.send(&format!(
        r#"{{"id":"unshadowed-source","command":"move_symbol","file":{},"symbol":"logger","destination":{}}}"#,
        crate::helpers::json_string(&source.display()),
        crate::helpers::json_string(&dest.display())
    ));
    assert_eq!(resp["success"], true, "move should succeed: {resp:?}");

    let source_content = std::fs::read_to_string(&source).expect("read source");
    // The source file has no other imports and no formatter config, so the new
    // statement takes the same default spelling add_import uses.
    assert!(
        source_content.contains("import { logger } from \"./dest\";"),
        "unshadowed source reference should import logger from dest:\n{source_content}"
    );
    assert!(
        source_content.contains("return logger();"),
        "remaining source code should still call logger:\n{source_content}"
    );

    aft.shutdown();
}

#[test]
fn move_symbol_does_not_reimport_object_keys_or_member_access() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let root = tmp.path().display().to_string();
    let source = tmp.path().join("source.ts");
    let dest = tmp.path().join("dest.ts");

    write_file(
        &source,
        r#"export function logger(): string {
  return 'moved';
}

export const config = { logger: 1 };

export function read(obj: { logger: number }): number {
  return obj.logger;
}
"#,
    );
    write_file(&dest, "export const existing = true;\n");

    let mut aft = AftProcess::spawn();
    configure(&mut aft, &root);

    let resp = aft.send(&format!(
        r#"{{"id":"object-member-source","command":"move_symbol","file":{},"symbol":"logger","destination":{}}}"#,
        crate::helpers::json_string(&source.display()),
        crate::helpers::json_string(&dest.display())
    ));
    assert_eq!(resp["success"], true, "move should succeed: {resp:?}");

    let source_content = std::fs::read_to_string(&source).expect("read source");
    assert!(
        !source_content.contains("from './dest'") && !source_content.contains("from \"./dest\""),
        "object keys/member access alone should not add an import from dest:\n{source_content}"
    );
    assert!(
        source_content.contains("{ logger: 1 }") && source_content.contains("obj.logger"),
        "object key and member access should remain in source:\n{source_content}"
    );

    aft.shutdown();
}

#[test]
fn move_symbol_preserves_default_import_shape_and_alias() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let root = tmp.path().display().to_string();
    let old = tmp.path().join("old.ts");
    let dest = tmp.path().join("new_home.ts");
    let consumer_foo = tmp.path().join("consumer_foo.ts");
    let consumer_bar = tmp.path().join("consumer_bar.ts");

    write_file(
        &old,
        "export default function Foo(): string {
  return 'foo';
}

export function keep(): string {
  return 'keep';
}
",
    );
    write_file(
        &dest,
        "export const existing = true;
",
    );
    write_file(
        &consumer_foo,
        "import Foo from './old';

export const value = Foo();
",
    );
    write_file(
        &consumer_bar,
        "import Bar from './old';

export const value = Bar();
",
    );

    let mut aft = AftProcess::spawn();
    configure(&mut aft, &root);

    let resp = aft.send(&format!(
        r#"{{"id":"default-imports","command":"move_symbol","file":{},"symbol":"Foo","destination":{}}}"#,
        crate::helpers::json_string(&old.display()),
        crate::helpers::json_string(&dest.display())
    ));
    assert_eq!(resp["success"], true, "move should succeed: {resp:?}");

    let foo_content = std::fs::read_to_string(&consumer_foo).expect("read consumer_foo");
    assert!(
        foo_content.contains("import Foo from './new_home';"),
        "default import should stay default for Foo local name:
{foo_content}"
    );
    assert!(
        !foo_content.contains("import { Foo }"),
        "default import must not become named import:
{foo_content}"
    );

    let bar_content = std::fs::read_to_string(&consumer_bar).expect("read consumer_bar");
    assert!(
        bar_content.contains("import Bar from './new_home';"),
        "default import alias Bar should be preserved:
{bar_content}"
    );
    assert!(
        !bar_content.contains("import { Foo }"),
        "aliased default must not become named import:
{bar_content}"
    );

    let dest_content = std::fs::read_to_string(&dest).expect("read dest");
    assert!(
        dest_content.contains("export default function Foo"),
        "destination should preserve default export:
{dest_content}"
    );

    aft.shutdown();
}

// ---------------------------------------------------------------------------
// Checkpoint tests
// ---------------------------------------------------------------------------

/// Checkpoint: move creates a checkpoint that can be listed and restored.
#[test]
fn move_symbol_checkpoint() {
    let (_tmp, root) = setup_move_fixture();
    let mut aft = AftProcess::spawn();
    configure(&mut aft, &root);

    let source = format!("{}/service.ts", root);
    let dest = format!("{}/utils.ts", root);

    // Snapshot originals
    let source_original = std::fs::read_to_string(&source).unwrap();
    let dest_original = std::fs::read_to_string(&dest).unwrap();
    let ca_original = std::fs::read_to_string(format!("{}/consumer_a.ts", root)).unwrap();

    // Perform the move
    let resp = aft.send(&format!(
        r#"{{"id":"1","command":"move_symbol","file":{},"symbol":"formatDate","destination":{}}}"#,
        crate::helpers::json_string(&source),
        crate::helpers::json_string(&dest)
    ));
    assert_eq!(resp["success"], true, "move should succeed: {:?}", resp);
    let checkpoint_name = resp["checkpoint_name"].as_str().unwrap().to_string();

    // Verify list_checkpoints shows it
    let resp = aft.send(r#"{"id":"2","command":"list_checkpoints"}"#);
    let checkpoints = resp["checkpoints"].as_array().expect("checkpoints array");
    let found = checkpoints
        .iter()
        .find(|c| c["name"].as_str() == Some(&checkpoint_name));
    assert!(
        found.is_some(),
        "checkpoint '{}' should appear in list_checkpoints, got: {:?}",
        checkpoint_name,
        checkpoints
    );
    let cp = found.unwrap();
    assert!(
        cp["file_count"].as_u64().unwrap() >= 2,
        "checkpoint should cover at least source + dest files"
    );

    // Restore the checkpoint
    let resp = aft.send(&format!(
        r#"{{"id":"3","command":"restore_checkpoint","name":"{}"}}"#,
        checkpoint_name
    ));
    assert_eq!(
        resp["name"].as_str(),
        Some(checkpoint_name.as_str()),
        "restore should return checkpoint name: {:?}",
        resp
    );

    // Verify files are back to their original state
    let source_restored = std::fs::read_to_string(&source).unwrap();
    let dest_restored = std::fs::read_to_string(&dest).unwrap();
    let ca_restored = std::fs::read_to_string(format!("{}/consumer_a.ts", root)).unwrap();

    assert_eq!(
        source_original, source_restored,
        "source should be restored to original"
    );
    assert_eq!(
        dest_original, dest_restored,
        "dest should be restored to original"
    );
    assert_eq!(
        ca_original, ca_restored,
        "consumer_a should be restored to original"
    );

    aft.shutdown();
}

#[test]
fn move_symbol_operation_undo_restores_source_destination_and_consumers() {
    let (_tmp, root) = setup_move_fixture();
    let mut aft = AftProcess::spawn();
    configure(&mut aft, &root);

    let source = format!("{}/service.ts", root);
    let dest = format!("{}/utils.ts", root);
    let consumer_a = format!("{}/consumer_a.ts", root);
    let consumer_e = format!("{}/features/consumer_e.ts", root);

    let source_original = std::fs::read_to_string(&source).unwrap();
    let dest_original = std::fs::read_to_string(&dest).unwrap();
    let consumer_a_original = std::fs::read_to_string(&consumer_a).unwrap();
    let consumer_e_original = std::fs::read_to_string(&consumer_e).unwrap();

    let resp = aft.send(&format!(
        r#"{{"id":"move-before-undo","command":"move_symbol","file":{},"symbol":"formatDate","destination":{}}}"#,
        crate::helpers::json_string(&source),
        crate::helpers::json_string(&dest)
    ));
    assert_eq!(resp["success"], true, "move should succeed: {resp:?}");
    assert!(
        resp["backup_ids"].as_array().unwrap().len() >= 4,
        "move should snapshot source, destination, and consumers: {resp:?}"
    );
    assert_ne!(std::fs::read_to_string(&source).unwrap(), source_original);
    assert_ne!(std::fs::read_to_string(&dest).unwrap(), dest_original);
    assert_ne!(
        std::fs::read_to_string(&consumer_a).unwrap(),
        consumer_a_original
    );
    assert_ne!(
        std::fs::read_to_string(&consumer_e).unwrap(),
        consumer_e_original
    );

    let undo = aft.send(r#"{"id":"undo-move-symbol-operation","command":"undo"}"#);
    assert_eq!(undo["success"], true, "undo should succeed: {undo:?}");
    assert_eq!(undo["operation"], true);
    assert!(
        undo["restored_count"].as_u64().unwrap() >= 4,
        "undo should restore all touched files: {undo:?}"
    );
    assert_eq!(std::fs::read_to_string(&source).unwrap(), source_original);
    assert_eq!(std::fs::read_to_string(&dest).unwrap(), dest_original);
    assert_eq!(
        std::fs::read_to_string(&consumer_a).unwrap(),
        consumer_a_original
    );
    assert_eq!(
        std::fs::read_to_string(&consumer_e).unwrap(),
        consumer_e_original
    );

    aft.shutdown();
}

#[test]
fn move_symbol_undo_removes_new_destination_file() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let root = tmp.path().display().to_string();
    let source = tmp.path().join("source.ts");
    let dest = tmp.path().join("new_dest.ts");
    write_file(
        &source,
        "export function keep() { return 1; }\nexport function moveMe() { return 2; }\n",
    );

    let source_original = std::fs::read_to_string(&source).unwrap();
    let mut aft = AftProcess::spawn();
    configure(&mut aft, &root);

    let resp = aft.send(&format!(
        r#"{{"id":"move-to-new-dest","command":"move_symbol","file":{},"symbol":"moveMe","destination":{}}}"#,
        crate::helpers::json_string(&source.display()),
        crate::helpers::json_string(&dest.display())
    ));
    assert_eq!(resp["success"], true, "move should succeed: {resp:?}");
    assert!(dest.exists(), "destination should be created");
    assert_ne!(std::fs::read_to_string(&source).unwrap(), source_original);

    let undo = aft.send(r#"{"id":"undo-move-new-dest","command":"undo"}"#);
    assert_eq!(undo["success"], true, "undo should succeed: {undo:?}");
    assert_eq!(std::fs::read_to_string(&source).unwrap(), source_original);
    assert!(
        !dest.exists(),
        "new destination should be deleted by tombstone undo"
    );

    aft.shutdown();
}

// ---------------------------------------------------------------------------
// Error path tests
// ---------------------------------------------------------------------------

/// `move_symbol` without prior `configure` returns `not_configured`.
#[test]
fn move_symbol_not_configured() {
    let (_tmp, root) = setup_move_fixture();
    let mut aft = AftProcess::spawn();

    // Use real files from the temp dir so the file_not_found guard passes,
    // but don't call configure — the not_configured guard fires next.
    let source = format!("{}/service.ts", root);
    let dest = format!("{}/utils.ts", root);

    let resp = aft.send(&format!(
        r#"{{"id":"1","command":"move_symbol","file":{},"symbol":"formatDate","destination":{}}}"#,
        crate::helpers::json_string(&source),
        crate::helpers::json_string(&dest)
    ));

    assert_eq!(resp["success"], false, "should fail: {:?}", resp);
    assert_eq!(resp["code"], "not_configured");

    aft.shutdown();
}

/// `move_symbol` for a nonexistent symbol returns `symbol_not_found`.
#[test]
fn move_symbol_symbol_not_found() {
    let (_tmp, root) = setup_move_fixture();
    let mut aft = AftProcess::spawn();
    configure(&mut aft, &root);

    let source = format!("{}/service.ts", root);
    let dest = format!("{}/utils.ts", root);

    let resp = aft.send(&format!(
        r#"{{"id":"1","command":"move_symbol","file":{},"symbol":"nonExistentFn","destination":{}}}"#,
        crate::helpers::json_string(&source),
        crate::helpers::json_string(&dest)
    ));

    assert_eq!(resp["success"], false, "should fail: {:?}", resp);
    assert_eq!(resp["code"], "symbol_not_found");

    aft.shutdown();
}

#[test]
fn move_symbol_ambiguous_symbol_is_error_response() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let source = tmp.path().join("source.ts");
    let dest = tmp.path().join("dest.ts");
    std::fs::write(
        &source,
        "export function duplicate(): string { return 'top'; }\nexport class Boxed {\n  duplicate(): string { return 'method'; }\n}\n",
    )
    .expect("write source");

    let mut aft = AftProcess::spawn();
    configure(&mut aft, &tmp.path().display().to_string());

    let resp = aft.send(&format!(
        r#"{{"id":"ambiguous","command":"move_symbol","file":{},"symbol":"duplicate","destination":{}}}"#,
        crate::helpers::json_string(&source.display()),
        crate::helpers::json_string(&dest.display())
    ));

    assert_eq!(resp["success"], false, "should fail: {resp:?}");
    assert_eq!(resp["code"], "ambiguous_symbol");
    assert!(resp["candidates"].as_array().unwrap().len() >= 2);

    aft.shutdown();
}

/// `move_symbol` rejects non-top-level symbols (class methods).
#[test]
fn move_symbol_non_top_level() {
    let (_tmp, root) = setup_move_fixture();
    let mut aft = AftProcess::spawn();
    configure(&mut aft, &root);

    let source = format!("{}/service.ts", root);
    let dest = format!("{}/utils.ts", root);

    // "format" is a method inside the DateHelper class in service.ts
    let resp = aft.send(&format!(
        r#"{{"id":"1","command":"move_symbol","file":{},"symbol":"format","destination":{},"scope":"DateHelper"}}"#,
        crate::helpers::json_string(&source),
        crate::helpers::json_string(&dest)
    ));

    assert_eq!(
        resp["success"], false,
        "should fail for class method: {:?}",
        resp
    );
    assert_eq!(
        resp["code"], "invalid_request",
        "should return invalid_request for non-top-level: {:?}",
        resp
    );
    assert!(
        resp["error"]
            .as_str()
            .unwrap_or("")
            .contains("non-top-level")
            || resp["message"]
                .as_str()
                .unwrap_or("")
                .contains("non-top-level"),
        "error message should mention non-top-level: {:?}",
        resp
    );

    aft.shutdown();
}

/// `move_symbol` with missing file returns file_not_found.
#[test]
fn move_symbol_file_not_found() {
    let (_tmp, root) = setup_move_fixture();
    let mut aft = AftProcess::spawn();
    configure(&mut aft, &root);

    let dest = format!("{}/utils.ts", root);

    let resp = aft.send(&format!(
        r#"{{"id":"1","command":"move_symbol","file":{},"symbol":"foo","destination":{}}}"#,
        crate::helpers::json_string(&format!("{}/nonexistent.ts", root)),
        crate::helpers::json_string(&dest)
    ));

    assert_eq!(resp["success"], false, "should fail: {:?}", resp);
    assert_eq!(resp["code"], "file_not_found");

    aft.shutdown();
}

/// Move of an exported symbol does not leave the `export` keyword behind.
///
/// Regression: when moving `export function greet`, the byte range of
/// `function_declaration` excludes the wrapping `export_statement`, so
/// `remove_symbol_from_source` only removed `function greet(...) {...}` and
/// left a stray `export` that then attached to the next sibling declaration.
/// `find_export_keyword_start` extends the deletion range backwards to cover
/// the `export` keyword and trailing whitespace.
#[test]
fn move_symbol_does_not_leak_export_keyword() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let source = tmp.path().join("sample.ts");
    let dest = tmp.path().join("helper.ts");

    std::fs::write(
        &source,
        "export function greet(user: string) {\n  return `Hello, ${user}!`;\n}\n\nfunction other(): number {\n  return 1;\n}\n",
    )
    .expect("write source");

    let mut aft = AftProcess::spawn();
    let root = tmp.path().display().to_string();
    let resp = aft.send(&format!(
        r#"{{"id":"cfg","command":"configure","harness":"opencode","project_root":{}}}"#,
        crate::helpers::json_string(&root)
    ));
    assert_eq!(resp["success"], true);

    let resp = aft.send(&format!(
        r#"{{"id":"1","command":"move_symbol","file":{},"symbol":"greet","destination":{}}}"#,
        crate::helpers::json_string(&source.display()),
        crate::helpers::json_string(&dest.display())
    ));
    assert_eq!(resp["success"], true, "move should succeed: {:?}", resp);

    let after_source = std::fs::read_to_string(&source).expect("read source");
    let after_dest = std::fs::read_to_string(&dest).expect("read dest");

    // Source: `other` should still NOT be exported. If the bug regressed,
    // `export ` would be left attached to `function other`.
    assert!(
        !after_source.contains("export function other"),
        "export keyword leaked onto the next declaration:\n{}",
        after_source
    );
    assert!(
        after_source.contains("function other(): number {"),
        "`other` should be present and unmodified:\n{}",
        after_source
    );
    assert!(
        !after_source.contains("greet"),
        "`greet` should be removed from source:\n{}",
        after_source
    );

    // Destination: `greet` should be exported (single export, not duplicated).
    assert!(
        after_dest.contains("export function greet"),
        "destination should have `export function greet`:\n{}",
        after_dest
    );
    assert!(
        !after_dest.contains("export export"),
        "destination should not have duplicate export:\n{}",
        after_dest
    );

    aft.shutdown();
}

/// Extract preserves the `export` keyword on the enclosing function.
///
/// Regression: insertion point was `function_declaration.start_byte()`, which
/// is AFTER the `export` keyword. The extracted function got inserted between
/// `export ` and `function`, silently transferring the `export` from the
/// original function to the new extracted one.
#[test]
fn extract_function_preserves_enclosing_export_keyword() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let file = tmp.path().join("sample.ts");

    std::fs::write(
        &file,
        "export function process(items: string[]) {\n  try {\n    const items2 = items.map(i => i.toLowerCase());\n    const message = `count: ${items2.length}`;\n    console.log(message);\n    return message;\n  } catch (e) {\n    throw new Error(`Failed: ${e}`);\n  }\n}\n",
    )
    .expect("write fixture");

    let mut aft = AftProcess::spawn();
    let root = tmp.path().display().to_string();
    let resp = aft.send(&format!(
        r#"{{"id":"cfg","command":"configure","harness":"opencode","project_root":{}}}"#,
        crate::helpers::json_string(&root)
    ));
    assert_eq!(resp["success"], true);

    // Extract just the items.map(...) line.
    let resp = aft.send(&format!(
        r#"{{"id":"1","command":"extract_function","file":{},"start_line":3,"end_line":4,"name":"makeItems"}}"#,
        crate::helpers::json_string(&file.display())
    ));
    assert_eq!(resp["success"], true, "extract should succeed: {:?}", resp);

    let after = std::fs::read_to_string(&file).expect("read file");
    // `process` must still be exported after extraction.
    assert!(
        after.contains("export function process"),
        "process should still be exported:\n{}",
        after
    );
    // The extracted function `makeItems` must NOT be exported.
    assert!(
        !after.contains("export function makeItems"),
        "extracted function should not be exported:\n{}",
        after
    );
    assert!(
        after.contains("function makeItems("),
        "extracted function should be present:\n{}",
        after
    );

    aft.shutdown();
}

#[test]
fn move_symbol_preserves_attributes_on_rewritten_imports() {
    let (_tmp, root) = setup_move_fixture();
    let consumer = std::path::Path::new(&root).join("consumer_a.ts");
    write_file(
        &consumer,
        "import { formatDate, parseDate } from './service' with { type: 'custom' };\n\
export const rendered = formatDate(new Date()) + parseDate('2026-07-26');\n",
    );

    let mut aft = AftProcess::spawn();
    configure(&mut aft, &root);
    let source = format!("{root}/service.ts");
    let destination = format!("{root}/utils.ts");
    let resp = aft.send(&format!(
        r#"{{"id":"move-attrs","command":"move_symbol","file":{},"symbol":"formatDate","destination":{}}}"#,
        crate::helpers::json_string(&source),
        crate::helpers::json_string(&destination)
    ));
    assert_eq!(resp["success"], true, "move should succeed: {resp:?}");

    let content = std::fs::read_to_string(&consumer).unwrap();
    assert!(
        content.contains("import { parseDate } from './service' with { type: 'custom' };")
            || content.contains("import { parseDate } from \"./service\" with { type: 'custom' };"),
        "remaining bindings must retain the source import attribute:\n{content}"
    );
    assert!(
        content.contains("import { formatDate } from './utils' with { type: 'custom' };")
            || content.contains("import { formatDate } from \"./utils\" with { type: 'custom' };"),
        "moved bindings must retain the rewritten import attribute:\n{content}"
    );

    aft.shutdown();
}
