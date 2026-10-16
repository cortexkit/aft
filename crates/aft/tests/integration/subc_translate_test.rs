#![cfg(unix)]

//! Cross-language parity gate for subc agent args -> native command translation.
//!
//! Feeds the golden fixtures captured from the current TypeScript OpenCode tool
//! wrappers (`scripts/capture-subc-parity.ts`) through `aft::subc_translate` and
//! asserts the native command payload matches byte-for-byte after deterministic
//! JSON key sorting.

use std::fs;
use std::path::{Path, PathBuf};

use aft::subc_translate::{subc_translate_with_context, TranslateContext};
use serde::Deserialize;
use serde_json::{json, Map, Value};

use super::helpers::AftProcess;

const PROJECT_ROOT_TOKEN: &str = "<PROJECT_ROOT>";

#[derive(Debug, Deserialize)]
struct TranslateInput {
    tool_name: String,
    agent_args: Value,
    project_root: String,
    diagnostics_on_edit: Option<bool>,
}

fn fixtures_root() -> PathBuf {
    crate::helpers::cargo_manifest_dir()
        .join("tests")
        .join("fixtures")
        .join("subc_parity")
        .join("translate")
}

fn setup_project_fixture(root: &Path) {
    if root.exists() {
        return;
    }
    fs::create_dir_all(root.join("src")).expect("create src fixture dir");
    fs::create_dir_all(root.join("docs")).expect("create docs fixture dir");
    fs::create_dir_all(root.join("packages/app")).expect("create package fixture dir");
    fs::write(root.join("README.md"), "# parity\n").expect("write README fixture");
    fs::write(root.join("src/main.ts"), "const value = 1;\n").expect("write main fixture");
    fs::write(root.join("docs/guide.md"), "# guide\n").expect("write docs fixture");
    fs::write(
        root.join("packages/app/index.tsx"),
        "export const App = () => null;\n",
    )
    .expect("write app fixture");
}

/// The fixture project lives in this test's private scratch dir, so concurrent
/// runs cannot share it and it is removed when the test ends. The goldens
/// store it as a token, so its exact location does not matter.
fn fixture_project_root() -> PathBuf {
    let root = crate::helpers::thread_scratch_dir()
        .join("aft-subc-parity")
        .join("project");
    setup_project_fixture(&root);
    root
}

fn project_root_for_input(raw: &str) -> PathBuf {
    if raw == PROJECT_ROOT_TOKEN {
        fixture_project_root()
    } else {
        PathBuf::from(raw)
    }
}

fn expand_project_root_tokens(value: Value, project_root: &Path) -> Value {
    let root = project_root.to_string_lossy();
    match value {
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|item| expand_project_root_tokens(item, project_root))
                .collect(),
        ),
        Value::Object(map) => {
            let mut out = Map::new();
            for (key, value) in map {
                out.insert(key, expand_project_root_tokens(value, project_root));
            }
            Value::Object(out)
        }
        Value::String(s) => Value::String(s.replace(PROJECT_ROOT_TOKEN, root.as_ref())),
        other => other,
    }
}

fn replace_project_root(value: Value, project_root: &Path) -> Value {
    let root = project_root.to_string_lossy();
    match value {
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|item| replace_project_root(item, project_root))
                .collect(),
        ),
        Value::Object(map) => {
            let mut out = Map::new();
            for (key, value) in map {
                out.insert(key, replace_project_root(value, project_root));
            }
            Value::Object(out)
        }
        Value::String(s) => Value::String(s.replace(root.as_ref(), PROJECT_ROOT_TOKEN)),
        other => other,
    }
}

fn sort_value(value: Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.into_iter().map(sort_value).collect()),
        Value::Object(map) => {
            let mut sorted = Map::new();
            let mut entries = map.into_iter().collect::<Vec<_>>();
            entries.sort_by(|(a, _), (b, _)| a.cmp(b));
            for (key, value) in entries {
                sorted.insert(key, sort_value(value));
            }
            Value::Object(sorted)
        }
        other => other,
    }
}

fn pretty_sorted(value: Value) -> String {
    format!(
        "{}\n",
        serde_json::to_string_pretty(&sort_value(value)).expect("serialize sorted JSON")
    )
}

fn assert_case(dir: &Path) -> Option<String> {
    let case = dir.file_name().unwrap().to_string_lossy().to_string();
    let input: TranslateInput =
        serde_json::from_str(&fs::read_to_string(dir.join("input.json")).expect("read input.json"))
            .expect("parse input.json");
    let project_root = project_root_for_input(&input.project_root);

    let ctx = TranslateContext {
        diagnostics_on_edit: input.diagnostics_on_edit.unwrap_or(false),
        preview: false,
        effective_hashline: false,
    };
    let agent_args = expand_project_root_tokens(input.agent_args, &project_root);
    let actual =
        match subc_translate_with_context(&input.tool_name, &agent_args, &project_root, ctx) {
            Ok(t) => json!({ "command": t.command, "args": t.args }),
            Err(err) => json!({ "error": { "code": err.code, "message": err.message } }),
        };
    let actual = pretty_sorted(replace_project_root(actual, &project_root));
    let expected = fs::read_to_string(dir.join("expected.json")).expect("read expected.json");
    if actual == expected {
        None
    } else {
        Some(format!(
            "case `{case}`:\n  actual:\n{actual}\n  expected:\n{expected}"
        ))
    }
}

#[test]
fn subc_translate_matches_typescript_golden_fixtures() {
    let root = fixtures_root();
    let mut cases: Vec<PathBuf> = fs::read_dir(&root)
        .unwrap_or_else(|e| panic!("read fixtures dir {}: {e}", root.display()))
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| p.is_dir())
        .collect();
    cases.sort();

    assert!(
        cases.len() >= 19,
        "expected >=19 translate parity fixtures, found {}",
        cases.len()
    );

    let failures = cases
        .iter()
        .filter_map(|dir| assert_case(dir))
        .collect::<Vec<_>>();
    assert!(
        failures.is_empty(),
        "{} translate parity mismatch(es):\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

#[test]
fn symbol_mode_validation_matches_shared_fixture_messages() {
    let fixture_path = crate::helpers::cargo_manifest_dir()
        .join("tests")
        .join("fixtures")
        .join("symbol-mode-validation.json");
    let cases: Value = serde_json::from_str(
        &fs::read_to_string(&fixture_path)
            .unwrap_or_else(|error| panic!("read {}: {error}", fixture_path.display())),
    )
    .expect("parse symbol-mode validation fixture");

    for case in cases.as_array().expect("fixture array") {
        let label = case["label"].as_str().expect("case label");
        let args = &case["arguments"];
        let expected = case["message"].as_str().expect("case message");
        let error = subc_translate_with_context(
            "edit",
            args,
            &fixture_project_root(),
            TranslateContext::default(),
        )
        .unwrap_err();
        assert_eq!(error.code, "invalid_request", "case {label}");
        assert_eq!(error.message, expected, "case {label}");
    }
}

#[test]
fn zoom_translate_matches_typescript_golden_fixtures() {
    let root = fixtures_root();
    let mut cases: Vec<PathBuf> = fs::read_dir(&root)
        .unwrap_or_else(|e| panic!("read fixtures dir {}: {e}", root.display()))
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| {
            p.is_dir()
                && p.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("zoom_"))
        })
        .collect();
    cases.sort();

    assert!(
        cases.len() >= 4,
        "expected >=4 zoom translate parity fixtures, found {}",
        cases.len()
    );

    let failures = cases
        .iter()
        .filter_map(|dir| assert_case(dir))
        .collect::<Vec<_>>();
    assert!(
        failures.is_empty(),
        "{} zoom translate parity mismatch(es):\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

#[test]
fn safety_translate_matches_typescript_golden_fixtures() {
    let root = fixtures_root();
    let mut cases: Vec<PathBuf> = fs::read_dir(&root)
        .unwrap_or_else(|e| panic!("read fixtures dir {}: {e}", root.display()))
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| {
            p.is_dir()
                && p.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("safety_"))
        })
        .collect();
    cases.sort();

    assert!(
        cases.len() >= 8,
        "expected >=8 safety translate parity fixtures, found {}",
        cases.len()
    );

    let failures = cases
        .iter()
        .filter_map(|dir| assert_case(dir))
        .collect::<Vec<_>>();
    assert!(
        failures.is_empty(),
        "{} safety translate parity mismatch(es):\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

#[test]
fn search_and_outline_translate_every_supported_include_tests_true_shape() {
    let root = fixture_project_root();
    for include_tests in [json!("true"), json!("1"), json!(1), json!(true)] {
        let search = subc_translate_with_context(
            "search",
            &json!({ "query": "fixture needle", "includeTests": include_tests.clone() }),
            &root,
            TranslateContext::default(),
        )
        .expect("translate search includeTests");
        assert_eq!(search.args.get("include_tests"), Some(&json!(true)));

        let outline = subc_translate_with_context(
            "outline",
            &json!({ "target": "src", "includeTests": include_tests }),
            &root,
            TranslateContext::default(),
        )
        .expect("translate outline includeTests");
        assert_eq!(outline.args.get("includeTests"), Some(&json!(true)));
    }
}

#[test]
fn main_dispatch_has_no_agent_edit_or_search_aliases() {
    let src = include_str!("../../src/main.rs");
    for pat in ["\"edit\" =>", "\"search\" =>"] {
        assert!(
            !src.contains(pat),
            "main::dispatch must not alias agent tool {pat}"
        );
    }
    assert!(src.contains("\"semantic_search\" =>"));
    assert!(src.contains("\"edit_match\" =>"));
    assert!(src.contains("\"outline\" =>"));
}

#[test]
fn file_urls_decode_to_local_paths_in_translate() {
    use aft::subc_translate::resolve_path_from_project_root;
    let root = std::path::Path::new(if cfg!(windows) { "C:\\proj" } else { "/proj" });

    // RFC 8089 spellings of the same local file all resolve identically.
    let plain = resolve_path_from_project_root(root, "/etc/hosts");
    assert_eq!(
        resolve_path_from_project_root(root, "file:///etc/hosts"),
        plain
    );
    assert_eq!(
        resolve_path_from_project_root(root, "file:/etc/hosts"),
        plain
    );
    assert_eq!(
        resolve_path_from_project_root(root, "file://localhost/etc/hosts"),
        plain
    );

    // Percent-encoded characters decode (space in a file name).
    let spaced = resolve_path_from_project_root(root, "file:///tmp/a%20b.md");
    assert_eq!(spaced, resolve_path_from_project_root(root, "/tmp/a b.md"));

    // Malformed-escape traversal: each VALID %HH decodes independently even
    // when a later escape (%ZZ) is malformed. %2e%2e -> .. must escape the
    // project just as a literal ../ would. This is the exact input the TS
    // decoder must agree with (decodeURIComponent would keep the whole string
    // encoded on the malformed %ZZ, diverging from this).
    let malformed = resolve_path_from_project_root(root, "file:///proj/%2e%2e/%ZZ/../secret");
    assert!(
        !malformed.starts_with(std::path::Path::new(if cfg!(windows) {
            "C:\\proj"
        } else {
            "/proj"
        })),
        "malformed-escape traversal must resolve OUT of /proj: {malformed:?}"
    );

    // A non-local authority is not a local path: left as-is on unix (becomes
    // a relative path under the root, matching the old rejected behavior)
    // rather than silently pointing at some other file.
    if cfg!(unix) {
        let unc = resolve_path_from_project_root(root, "file://server/share/x.txt");
        assert!(
            unc.starts_with(root),
            "non-local authority must not decode on unix: {unc:?}"
        );
    }

    // Windows drive-letter form.
    if cfg!(windows) {
        let drive = resolve_path_from_project_root(root, "file:///C:/temp/x.txt");
        assert_eq!(drive, std::path::PathBuf::from("C:\\temp\\x.txt"));
    }
}

// The GPT 5.6 Terra report shape (GitHub #171): every optional edit field
// carries a type-default sentinel and the real payload lives in one field.
// The all-empty `edits` array must not claim the edits mode, so the request
// resolves to a successful appendContent write on disk.
#[test]
fn edit_all_empty_sentinel_edits_resolves_to_append_write() {
    let dir = tempfile::tempdir().expect("temp project");
    let root = dir.path();
    let target = root.join("sentinel.txt");
    fs::write(&target, "before\n").expect("write fixture");

    let mut aft = AftProcess::spawn();
    let configure = aft.configure(root);
    assert_eq!(
        configure["success"], true,
        "configure failed: {configure:#}"
    );

    let response = aft.send(
        &json!({
            "id": "edit-sentinel-append",
            "command": "tool_call",
            "session_id": "subc-translate-sentinel",
            "name": "edit",
            "arguments": {
                "path": "sentinel.txt",
                "symbol": "",
                "content": "",
                "appendContent": "CONTENT IT APPENDS",
                "edits": [
                    { "oldString": "", "newString": "", "replaceAll": false,
                      "occurrence": 1, "startLine": 1, "endLine": 1, "content": "" }
                ]
            }
        })
        .to_string(),
    );

    assert_eq!(
        response["success"], true,
        "sentinel edits must resolve to an append write: {response:#}"
    );
    assert_eq!(
        fs::read_to_string(&target).expect("read target"),
        "before\nCONTENT IT APPENDS",
        "appendContent must be written to disk"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

// {oldString:"", newString:"x"} is NOT a sentinel: it is kept as an edits
// claim so the batch parser reports its specific empty-match error instead of
// a conflicting-modes error. The error must mention the match problem, not
// the mode conflict.
#[test]
fn edit_empty_old_string_surfaces_batch_match_error() {
    let dir = tempfile::tempdir().expect("temp project");
    let root = dir.path();
    let target = root.join("empty-match.txt");
    fs::write(&target, "some content\n").expect("write fixture");

    let mut aft = AftProcess::spawn();
    let configure = aft.configure(root);
    assert_eq!(
        configure["success"], true,
        "configure failed: {configure:#}"
    );

    let response = aft.send(
        &json!({
            "id": "edit-empty-match",
            "command": "tool_call",
            "session_id": "subc-translate-sentinel",
            "name": "edit",
            "arguments": {
                "path": "empty-match.txt",
                "edits": [{ "oldString": "", "newString": "x" }]
            }
        })
        .to_string(),
    );

    assert_eq!(
        response["success"], false,
        "empty oldString must fail at the batch leaf: {response:#}"
    );
    let message = response["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("match") || message.contains("oldString"),
        "expected a match/oldString problem, got: {message}"
    );
    assert!(
        !message.contains("conflicting modes"),
        "must not be a conflicting-modes error: {message}"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn edit_sentinel_item_alongside_real_match_applies_batch() {
    let dir = tempfile::tempdir().expect("temp project");
    let root = dir.path();
    let target = root.join("mixed.txt");
    fs::write(&target, "one two\n").expect("write fixture");

    let mut aft = AftProcess::spawn();
    let configure = aft.configure(root);
    assert_eq!(
        configure["success"], true,
        "configure failed: {configure:#}"
    );

    let response = aft.send(
        &json!({
            "id": "edit-sentinel-mixed",
            "command": "tool_call",
            "session_id": "subc-translate-sentinel",
            "name": "edit",
            "arguments": {
                "path": "mixed.txt",
                "edits": [
                    { "oldString": "", "newString": "", "replaceAll": false,
                      "occurrence": 1, "startLine": 1, "endLine": 1, "content": "" },
                    { "oldString": "one", "newString": "ONE" }
                ]
            }
        })
        .to_string(),
    );

    assert_eq!(
        response["success"], true,
        "real item must survive sentinel stripping: {response:#}"
    );
    assert_eq!(
        fs::read_to_string(&target).expect("read target"),
        "ONE two\n",
        "the real match item must be applied to disk"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn effective_hashline_edit_routes_before_legacy_translation() {
    let translated = subc_translate_with_context(
        "edit",
        &json!({ "patch": "[src/main.ts#ABCD]\nPUT 1\nconst value = 2;\n" }),
        Path::new("/project"),
        TranslateContext {
            diagnostics_on_edit: false,
            preview: true,
            effective_hashline: true,
        },
    )
    .expect("hashline patch should translate");

    assert_eq!(translated.command, "hashline_edit");
    assert_eq!(
        Value::Object(translated.args),
        json!({
            "patch": "[src/main.ts#ABCD]\nPUT 1\nconst value = 2;\n",
            "preview": true,
        })
    );
}

#[test]
fn hashline_edit_without_the_carrier_falls_to_the_legacy_arm() {
    // A downgraded plugin never sends the hashline carrier, so `effective_hashline`
    // is false and translation must take the legacy path — both for a legacy edit
    // shape (which has to work) and for a patch (which must not be silently
    // accepted by a session that has no tags to address).
    let legacy = subc_translate_with_context(
        "edit",
        &json!({ "path": "src/main.ts", "oldString": "1", "newString": "2" }),
        Path::new("/project"),
        TranslateContext {
            diagnostics_on_edit: false,
            preview: false,
            effective_hashline: false,
        },
    )
    .expect("legacy edit shape translates without the carrier");
    // Normalization folds the top-level find/replace into `edits`, so the legacy
    // arm lands on the batch command rather than the hashline one.
    assert_eq!(legacy.command, "batch");

    let patch = subc_translate_with_context(
        "edit",
        &json!({ "patch": "[src/main.ts#ABCD]\nPUT 1\nconst value = 2;\n" }),
        Path::new("/project"),
        TranslateContext {
            diagnostics_on_edit: false,
            preview: false,
            effective_hashline: false,
        },
    )
    .expect_err("a patch must not reach the hashline arm without the carrier");
    assert_eq!(patch.code, "invalid_request");
    assert!(
        !patch.message.contains("hashline"),
        "legacy arm should not steer toward hashline: {}",
        patch.message
    );
}

#[test]
fn effective_hashline_edit_rejects_every_legacy_key_with_hashline_steering() {
    let error = subc_translate_with_context(
        "edit",
        &json!({ "filePath": "src/main.ts", "oldString": "1", "newString": "2" }),
        Path::new("/project"),
        TranslateContext {
            diagnostics_on_edit: false,
            preview: false,
            effective_hashline: true,
        },
    )
    .expect_err("legacy edit shape must not reach legacy translation");

    assert_eq!(error.code, "hashline_parse_error");
    assert!(error.message.contains("hashline patch"));
    assert!(!error.message.contains("requires exactly one edit mode"));
}

fn placeholder_tool_call(aft: &mut AftProcess, id: &str, name: &str, arguments: Value) -> Value {
    aft.send(
        &json!({
            "id": id,
            "command": "tool_call",
            "session_id": "subc-translate-placeholders",
            "name": name,
            "arguments": arguments,
        })
        .to_string(),
    )
}

#[test]
fn relative_import_operations_edit_the_project_root_file() {
    let dir = tempfile::tempdir().expect("temp project");
    let root = dir.path();
    let target = root.join("src/import-target.ts");
    fs::create_dir_all(target.parent().expect("target parent")).expect("create src");
    fs::write(
        &target,
        "import { beta, alpha } from \"pkg\";\nimport { zeta, aardvark } from \"other\";\nconst answer = 42;\n",
    )
    .expect("write import fixture");

    let mut aft = AftProcess::spawn();
    let configure = aft.configure(root);
    assert_eq!(configure["success"], true, "configure: {configure:#}");

    let add = placeholder_tool_call(
        &mut aft,
        "relative-import-add",
        "import",
        json!({
            "op": "add",
            "filePath": "src/import-target.ts",
            "module": "pkg",
            "names": ["gamma"]
        }),
    );
    assert_eq!(add["success"], true, "add import: {add:#}");
    let after_add = fs::read_to_string(&target).expect("read after add");
    assert!(
        after_add.contains("gamma"),
        "add edited wrong file: {after_add}"
    );

    let remove = placeholder_tool_call(
        &mut aft,
        "relative-import-remove",
        "import",
        json!({
            "op": "remove",
            "filePath": "src/import-target.ts",
            "module": "pkg",
            "removeName": "beta"
        }),
    );
    assert_eq!(remove["success"], true, "remove import: {remove:#}");
    let after_remove = fs::read_to_string(&target).expect("read after remove");
    assert!(
        !after_remove.contains("beta"),
        "remove edited wrong file: {after_remove}"
    );
    assert!(
        after_remove.contains("gamma"),
        "remove damaged added import: {after_remove}"
    );

    let before_organize = after_remove;
    let organize = placeholder_tool_call(
        &mut aft,
        "relative-import-organize",
        "import",
        json!({ "op": "organize", "filePath": "src/import-target.ts" }),
    );
    assert_eq!(organize["success"], true, "organize imports: {organize:#}");
    let after_organize = fs::read_to_string(&target).expect("read after organize");
    assert_ne!(
        after_organize, before_organize,
        "organize must edit the project-root file"
    );
    assert!(after_organize.contains("aardvark"));
    assert!(after_organize.contains("gamma"));

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn tool_call_read_http_path_explains_url_fetching_is_unsupported() {
    let dir = tempfile::tempdir().unwrap();
    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);
    for key in ["path", "filePath"] {
        let response = placeholder_tool_call(
            &mut aft,
            "read-http-path",
            "read",
            json!({ key: "https://example.test/docs" }),
        );
        assert_eq!(response["success"], false, "{response:#}");
        assert_eq!(response["code"], "invalid_request", "{response:#}");
        let message = response["message"].as_str().unwrap_or_default();
        assert!(
            message.contains("read does not fetch HTTP/HTTPS URLs"),
            "{message}"
        );
        assert!(message.contains("https://example.test/docs"), "{message}");
        assert!(!message.contains("file not found"), "{message}");
    }
    assert!(aft.shutdown().success());
}

#[test]
fn tool_call_zoom_conflicting_http_locations_name_both_values() {
    let dir = tempfile::tempdir().unwrap();
    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);
    let path = "https://example.test/path";
    let url = "https://example.test/url";
    for args in [
        json!({ "path": path, "url": url, "symbols": "Heading" }),
        json!({ "targets": [{ "path": path, "url": url, "symbol": "Heading" }] }),
    ] {
        let response = placeholder_tool_call(&mut aft, "zoom-conflict", "zoom", args);
        assert_eq!(response["success"], false, "{response:#}");
        assert_eq!(response["code"], "invalid_request", "{response:#}");
        let message = response["message"].as_str().unwrap_or_default();
        assert!(message.contains(path) && message.contains(url), "{message}");
    }
    assert!(aft.shutdown().success());
}

#[test]
fn zoom_and_outline_errors_name_agent_location_parameters() {
    let dir = tempfile::tempdir().unwrap();
    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);
    let response = placeholder_tool_call(
        &mut aft,
        "zoom-missing-path",
        "zoom",
        json!({ "path": "missing.rs", "symbols": "client" }),
    );
    assert_eq!(
        response["targets"][0]["response"]["code"], "file_not_found",
        "{response:#}"
    );
    let message = response["text"].as_str().unwrap_or_default();
    assert!(
        message.contains("Set `target` to an existing file path"),
        "{message}"
    );
    assert!(!message.contains("`file`"), "{message}");

    let response =
        aft.send(&json!({ "id": "outline-missing-target", "command": "outline" }).to_string());
    assert_eq!(response["success"], false, "{response:#}");
    assert_eq!(
        response["message"],
        "outline: missing required param 'target'"
    );
    assert!(aft.shutdown().success());
}

// Replays a `read` call recorded from a model that fills every property:
// the line range is inverted, and offset/limit carry their own values. It
// used to come back as an empty SUCCESS, and a reader concluded code "was not
// there" that sat in exactly those lines. It must now be refused by name.
#[test]
fn tool_call_read_refuses_recorded_inverted_range() {
    let dir = tempfile::tempdir().expect("temp project");
    let root = dir.path();
    let content = (1..=300).map(|n| format!("line {n}\n")).collect::<String>();
    fs::write(root.join("conversion-lane.ts"), content).expect("write fixture");

    let mut aft = AftProcess::spawn();
    let configure = aft.configure(root);
    assert_eq!(configure["success"], true, "configure: {configure:#}");

    let response = placeholder_tool_call(
        &mut aft,
        "read-inverted",
        "read",
        json!({
            "filePath": "conversion-lane.ts",
            "startLine": 245,
            "endLine": 145,
            "offset": 245,
            "limit": 70
        }),
    );
    assert_eq!(response["success"], false, "{response:#}");
    assert_eq!(response["code"], "invalid_range", "{response:#}");
    let message = response["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("startLine 245 is after endLine 145"),
        "{response:#}"
    );

    // A start past the last line is refused the same way.
    let response = placeholder_tool_call(
        &mut aft,
        "read-past-end",
        "read",
        json!({ "filePath": "conversion-lane.ts", "startLine": 400, "endLine": 0, "offset": 0, "limit": 0 }),
    );
    assert_eq!(response["success"], false, "{response:#}");
    assert!(
        response["message"]
            .as_str()
            .unwrap_or_default()
            .contains("startLine 400 is past the end of the file (300 lines)"),
        "{response:#}"
    );

    // Zero offset/limit placeholders next to a real startLine/endLine range
    // still read that range.
    let response = placeholder_tool_call(
        &mut aft,
        "read-real-range",
        "read",
        json!({ "filePath": "conversion-lane.ts", "startLine": 145, "endLine": 147, "offset": 0, "limit": 0 }),
    );
    assert_eq!(response["success"], true, "{response:#}");
    let text = response["text"].as_str().unwrap_or_default();
    assert!(
        text.contains("145: line 145") && text.contains("147: line 147"),
        "{text}"
    );

    assert!(aft.shutdown().success());
}

// Replays the `aft_zoom` shapes recorded from a model that fills every
// property: a real `path` + `symbols` request arrives next to a placeholder
// `targets` entry (`x`/`x`) or an empty one. The real request must be
// answered, and the unresolvable `x` target must report its own error line.
#[test]
fn tool_call_zoom_merges_recorded_placeholder_target_shapes() {
    let dir = tempfile::tempdir().expect("temp project");
    let root = dir.path();
    fs::write(
        root.join("lane.ts"),
        "export function alpha() {\n  return 1;\n}\n\nexport function beta() {\n  return 2;\n}\n",
    )
    .expect("write fixture");

    let mut aft = AftProcess::spawn();
    let configure = aft.configure(root);
    assert_eq!(configure["success"], true, "configure: {configure:#}");

    let response = placeholder_tool_call(
        &mut aft,
        "zoom-x-target",
        "zoom",
        json!({
            "path": "lane.ts",
            "symbols": ["alpha", "beta"],
            "url": "",
            "targets": { "path": "x", "symbol": "x" },
            "contextLines": 0,
            "callgraph": false
        }),
    );
    assert_eq!(response["success"], true, "{response:#}");
    let text = response["text"].as_str().unwrap_or_default();
    assert!(text.contains("function alpha()"), "{text}");
    assert!(text.contains("function beta()"), "{text}");
    assert!(text.contains("Symbol \"x\" not found in x:"), "{text}");
    assert!(
        text.contains("Incomplete zoom results: one or more symbols failed."),
        "{text}"
    );

    for targets in [
        json!({ "path": "", "symbol": "" }),
        json!([{ "path": " ", "symbol": "" }]),
        json!([]),
    ] {
        let response = placeholder_tool_call(
            &mut aft,
            "zoom-empty-target",
            "zoom",
            json!({ "path": "lane.ts", "symbols": "alpha", "url": "", "targets": targets }),
        );
        assert_eq!(response["success"], true, "{targets}: {response:#}");
        let text = response["text"].as_str().unwrap_or_default();
        assert!(text.contains("function alpha()"), "{targets}: {text}");
        assert!(!text.contains("Incomplete"), "{targets}: {text}");
    }

    // One usable target next to one missing its symbol: the usable one is
    // answered, the other reports its own line.
    let response = placeholder_tool_call(
        &mut aft,
        "zoom-partial-target",
        "zoom",
        json!({ "targets": [
            { "path": "lane.ts", "symbol": "beta" },
            { "path": "lane.ts", "symbol": "" }
        ] }),
    );
    assert_eq!(response["success"], true, "{response:#}");
    let text = response["text"].as_str().unwrap_or_default();
    assert!(text.contains("function beta()"), "{text}");
    assert!(
        text.contains("batch[1].symbols must be a non-empty string"),
        "{text}"
    );

    // When every zoom input is a blank placeholder, the call is refused.
    let response = placeholder_tool_call(
        &mut aft,
        "zoom-nothing",
        "zoom",
        json!({ "path": "", "symbols": "", "url": "", "targets": { "path": "", "symbol": "" } }),
    );
    assert_eq!(response["success"], false, "{response:#}");

    assert!(aft.shutdown().success());
}

// Benchmark harnesses detect split-query support by sending `pattern: "["`
// and expecting grep's stable `invalid_pattern` code. Pin that contract on
// both entry points: the native NDJSON command and the translated tool call.
#[test]
fn search_invalid_pattern_refuses_with_invalid_pattern_code_on_both_paths() {
    let dir = tempfile::tempdir().expect("temp project");
    let root = dir.path();
    fs::write(root.join("lib.rs"), "pub fn load() {}\n").expect("write fixture");

    let mut aft = AftProcess::spawn();
    let configure = aft.configure(root);
    assert_eq!(
        configure["success"], true,
        "configure failed: {configure:#}"
    );

    let native = aft.send(
        &json!({
            "id": "search-invalid-pattern-native",
            "command": "semantic_search",
            "pattern": "[",
        })
        .to_string(),
    );
    let translated = aft.send(
        &json!({
            "id": "search-invalid-pattern-subc",
            "command": "tool_call",
            "session_id": "search-invalid-pattern",
            "name": "search",
            "arguments": { "query": "where is config loaded", "pattern": "[" }
        })
        .to_string(),
    );
    for response in [native, translated] {
        assert_eq!(response["success"], false, "{response:#}");
        assert_eq!(response["code"], "invalid_pattern", "{response:#}");
        let message = response["message"].as_str().unwrap_or_default();
        assert!(
            message.contains("regex parse error") && message.contains('^'),
            "grep's error text with its position: {response:#}"
        );
    }

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn tool_call_zoom_batch_errors_keep_original_entry_index_after_symbol_expansion() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("lane.ts"),
        "export function alpha() {}\nexport function beta() {}\n",
    )
    .unwrap();
    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);
    let response = placeholder_tool_call(
        &mut aft,
        "zoom-batch-index",
        "zoom",
        json!({ "batch": [
        { "target": "lane.ts", "symbols": ["alpha", "beta"] },
        { "target": "lane.ts", "symbols": [] }
    ] }),
    );
    let text = response["text"].as_str().unwrap_or_default();
    assert!(
        text.contains("function alpha()") && text.contains("function beta()"),
        "{response:#}"
    );
    assert!(
        text.contains("batch[1].symbols must be a non-empty string"),
        "{response:#}"
    );
    assert!(!text.contains("batch[2].symbols"), "{response:#}");
    assert!(aft.shutdown().success());
}
