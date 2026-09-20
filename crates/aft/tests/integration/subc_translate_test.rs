#![cfg(unix)]

//! Cross-language parity gate for subc agent args -> native command translation.
//!
//! Feeds the golden fixtures captured from the current TypeScript OpenCode tool
//! wrappers (`scripts/capture-subc-parity.ts`) through `aft::subc_translate` and
//! asserts the native command payload matches byte-for-byte after deterministic
//! JSON key sorting.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Once;

use aft::subc_translate::{subc_translate_with_context, TranslateContext};
use serde::Deserialize;
use serde_json::{json, Map, Value};

use super::helpers::AftProcess;

static PROJECT_FIXTURE: Once = Once::new();
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
    PROJECT_FIXTURE.call_once(|| {
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
    });
}

fn fixture_project_root() -> PathBuf {
    std::env::temp_dir().join("aft-subc-parity").join("project")
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
    setup_project_fixture(&project_root);

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
