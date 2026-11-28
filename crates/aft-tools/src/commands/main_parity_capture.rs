//! Main-pinned, whole-response compatibility recording for non-Luau checkouts.
//!
//! Regeneration uses the same test-only constant-vector callback and command
//! handlers in both checkouts. Production code and serializers are unchanged.

use crate::config::{Config, IndexesConfig, SemanticBackend};
use crate::context::AppContext;
use crate::protocol::{RawRequest, Response};
use aft_engine::test_bridge::display_data::meaning::use_constant_vectors;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const MAIN: &str = "1c6563b4c965d48c92937d6675cdbca333f37811";

// Each normalized field is process-specific; no product field is omitted.
const TIMINGS: &[(&str, &str)] = &[
    ("duration_ms", "wall-clock command/job execution duration"),
    ("elapsed_ms", "wall-clock elapsed execution time"),
    ("scan_time_ms", "wall-clock source scan duration"),
    ("analysis_time_ms", "wall-clock analysis duration"),
    ("started_at_ms", "process-observed job start timestamp"),
    ("finished_at_ms", "process-observed job finish timestamp"),
    (
        "last_progress_at_ms",
        "process-observed fill progress timestamp",
    ),
    ("since_ms", "process-observed activity start timestamp"),
    ("search_ms", "wall-clock grep search duration"),
    ("tier2_last_run", "process-observed inspect scan timestamp"),
];

fn normalize_timings(value: &mut Value) {
    match value {
        Value::Object(fields) => {
            for (name, value) in fields {
                if TIMINGS.iter().any(|(field, _)| name == field) {
                    assert!(
                        value.is_number() || value.is_null(),
                        "unexpected timing field {name}: {value}"
                    );
                    if value.is_number() {
                        *value = json!(0);
                    }
                } else {
                    normalize_timings(value);
                }
            }
        }
        Value::Array(items) => {
            for value in items {
                normalize_timings(value);
            }
        }
        _ => (),
    }
}

fn recorded_response(base: &Path, response: Response) -> String {
    let raw = serde_json::to_string(&response).unwrap();
    let mut parsed: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(
        serde_json::to_string(&parsed).unwrap(),
        raw,
        "recording must preserve untouched bytes and key order"
    );
    normalize_timings(&mut parsed);
    if let Some(generation) = parsed.pointer_mut("/structuredContent/plan/snapshot_generation") {
        assert!(
            generation.is_string(),
            "expected a publication identity: {generation}"
        );
        *generation = json!("<publication-generation>");
    }
    // The temporary checkout/storage prefix differs between capture processes.
    serde_json::to_string(&parsed)
        .unwrap()
        .replace(base.to_str().unwrap(), "<fixture>")
}

fn request(command: &str, params: Value) -> RawRequest {
    RawRequest {
        id: "main-parity".into(),
        command: command.into(),
        lsp_hints: None,
        session_id: None,
        params,
    }
}

fn ask(ctx: &AppContext, command: &str, params: Value) -> Response {
    let req = request(command, params);
    match command {
        "outline" => crate::commands::outline::handle_outline(&req, ctx),
        "zoom" => crate::commands::zoom::handle_zoom(&req, ctx),
        "semantic_search" => crate::commands::semantic_search::handle_semantic_search(&req, ctx),
        "grep" => crate::commands::grep::handle_grep(&req, ctx),
        "inspect" => crate::commands::inspect::handle_inspect(&req, ctx),
        "callers" => crate::commands::callers::handle_callers(&req, ctx),
        "impact" => crate::commands::impact::handle_impact(&req, ctx),
        "call_tree" => crate::commands::call_tree::handle_call_tree(&req, ctx),
        "trace_to" => crate::commands::trace_to::handle_trace_to(&req, ctx),
        "trace_to_symbol" => crate::commands::trace_to_symbol::handle_trace_to_symbol(&req, ctx),
        "trace_data" => crate::commands::trace_data::handle_trace_data(&req, ctx),
        _ => panic!("unhandled capture command {command}"),
    }
}

fn settled<T>(mut read: impl FnMut() -> Option<T>, what: &str) -> T {
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        if let Some(value) = read() {
            return value;
        }
        assert!(Instant::now() < deadline, "{what} did not settle");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn write_fixture(path: &Path, bytes: impl AsRef<[u8]>) {
    let bytes = bytes.as_ref();
    if std::fs::read(path).ok().as_deref() != Some(bytes) {
        std::fs::write(path, bytes).unwrap();
    }
    // Grep sorts by mtime before path. Equal, fixed input times keep that
    // product ordering contract identical across independently built fixtures.
    let modified = std::time::UNIX_EPOCH + Duration::from_secs(1_000);
    if std::fs::metadata(path).unwrap().modified().ok() != Some(modified) {
        filetime::set_file_mtime(path, filetime::FileTime::from_unix_time(1_000, 0)).unwrap();
    }
}

fn capture_case(
    base: &Path,
    invalid_utf8: bool,
    lines: &mut Vec<String>,
    raw_lines: &mut Vec<String>,
) {
    let name = if invalid_utf8 {
        "invalid-utf8"
    } else {
        "clean"
    };
    let directory = base.join(name);
    let root = directory.join("project");
    let storage = directory.join("storage");
    std::fs::create_dir_all(&root).unwrap();
    if !root.join(".git").exists() {
        let status = std::process::Command::new("git")
            .args(["init", "--quiet", "--initial-branch=parity", "--template="])
            .arg(&root)
            .status()
            .unwrap();
        assert!(status.success());
    }
    write_fixture(&root.join("a.ts"), "// TODO: parity marker\n/** Calculates a searchable input value. */\nexport function target(value: number) { return value + 1; }\nexport function origin(input: number) { const copied = input; return target(copied); }\n");
    write_fixture(&root.join("b.ts"), "import { target } from './a';\nexport function caller(input: number) { return target(input); }\n");
    write_fixture(
        &root.join("package.json"),
        "{\"name\":\"parity\",\"version\":\"1.0.0\",\"exports\":\"./b.ts\"}\n",
    );
    if invalid_utf8 {
        write_fixture(&root.join("b.rs"), b"pub fn broken() {\n //\xff\n}\n");
    }
    use_constant_vectors(root.clone());
    let mut config = Config {
        project_root: Some(root.clone()),
        storage_dir: Some(storage.clone()),
        cache_dir_opt_out: true,
        indexes: IndexesConfig {
            trigram: true,
            semantic: true,
            callgraph: true,
        },
        ..Config::default()
    };
    config.semantic.backend = SemanticBackend::OpenAiCompatible;
    config.semantic.model = "main-parity".into();
    config.semantic.base_url = Some("http://127.0.0.1:1".into());
    // Leave unset optional settings out: a user file may not hold null.
    let mut semantic = serde_json::to_value(&config.semantic).unwrap();
    semantic
        .as_object_mut()
        .unwrap()
        .retain(|_, value| !value.is_null());
    let document = json!({"indexes":config.indexes,"semantic":semantic,"inspect":{"categories":{"diagnostics":false}}}).to_string();
    let ctx = crate::test_context(Box::new(crate::parser::TreeSitterProvider::new()), config);
    ctx.set_canonical_cache_root(root.clone());
    crate::semantic_admission::note_opened("main-parity", &root);
    let configured = crate::commands::configure::handle_configure(
        &request(
            "configure",
            json!({
                "harness":"opencode", "project_root":root,"storage_dir":storage,
                "config":[{"tier":"user","source":directory.join("aft.jsonc"),"doc":document}]
            }),
        ),
        &ctx,
    );
    assert!(configured.success, "{:?}", configured.data);
    ctx.data_checkout()
        .unwrap()
        .wait_for_memory_profile_idle_for_test();
    let meaning = settled(
        || {
            crate::data_layer::meaning_query::DataSemantic::current(&ctx)
                .and_then(|data| data.serving().cloned())
                .filter(|meaning| meaning.coverage().pending.is_empty())
        },
        "constant-vector fill",
    );
    if invalid_utf8 {
        assert!(
            meaning
                .coverage()
                .files_failed
                .iter()
                .any(|(path, reason)| path == &root.join("b.rs")
                    && reason.to_lowercase().contains("utf-8")),
            "{:?}",
            meaning.coverage()
        );
    }
    settled(
        || {
            let response = ask(&ctx, "grep", json!({"pattern":"parity"}));
            (response.data["index"]["trigram"]["status"] == "ready").then_some(())
        },
        "trigram publication",
    );

    let a = root.join("a.ts");
    let b = root.join("b.ts");
    let requests = [
        ("outline-single", "outline", json!({"file":a})),
        (
            "outline-files",
            "outline",
            json!({"file":root,"files":true}),
        ),
        ("outline-directory", "outline", json!({"directory":root})),
        (
            "zoom",
            "zoom",
            json!({"file":a,"symbol":"target","include_callgraph":true}),
        ),
        (
            "zoom-batch",
            "zoom",
            json!({"targets":[{"file":a,"symbols":"target"},{"file":b,"symbols":"caller"}],"include_callgraph":true}),
        ),
        (
            "search-identifier",
            "semantic_search",
            json!({"query":"target","top_k":5}),
        ),
        (
            "search-meaning",
            "semantic_search",
            json!({"query":"searchable input value","top_k":5}),
        ),
        ("grep", "grep", json!({"pattern":"parity|TODO"})),
        (
            "inspect-todos",
            "inspect",
            json!({"scope":[root],"sections":["todos"]}),
        ),
        (
            "inspect-dead-code",
            "inspect",
            json!({"sections":["dead_code"]}),
        ),
        ("callers", "callers", json!({"file":a,"symbol":"target"})),
        ("impact", "impact", json!({"file":a,"symbol":"target"})),
        (
            "call-tree",
            "call_tree",
            json!({"file":b,"symbol":"caller"}),
        ),
        ("trace-to", "trace_to", json!({"file":a,"symbol":"target"})),
        (
            "trace-to-symbol",
            "trace_to_symbol",
            json!({"file":b,"symbol":"caller","toSymbol":"target","toPath":a}),
        ),
        (
            "trace-data",
            "trace_data",
            json!({"file":b,"symbol":"caller","expression":"input"}),
        ),
    ];
    for (label, command, params) in requests {
        let response = if command == "semantic_search" {
            let query = params["query"].as_str().unwrap().to_string();
            crate::commands::semantic_search::with_precomputed_query_vector(
                &query,
                vec![1.0, 0.0],
                || ask(&ctx, command, params),
            )
        } else if command == "inspect" {
            settled(
                || {
                    let response = ask(&ctx, command, params.clone());
                    (response.data["code"] != "inspect_not_fresh").then_some(response)
                },
                "inspect freshness",
            )
        } else {
            ask(&ctx, command, params)
        };
        assert!(response.success, "{name}/{label}: {:?}", response.data);
        raw_lines.push(format!(
            "{name}/{label}\t{}",
            serde_json::to_string(&response).unwrap()
        ));
        lines.push(format!(
            "{name}/{label}\t{}",
            recorded_response(base, response)
        ));
    }
}

#[test]
fn luau_s4_main_pinned_serialized_responses_match() {
    let temp = tempfile::tempdir().unwrap();
    let directory = std::env::var_os("AFT_MAIN_PARITY_FIXTURE")
        .map(PathBuf::from)
        .unwrap_or_else(|| temp.path().to_path_buf());
    std::fs::create_dir_all(&directory).unwrap();
    let base = std::fs::canonicalize(&directory).unwrap();
    let mut lines = vec![format!("# main_commit={MAIN}")];
    let mut raw_lines = vec![format!("# reference_main={MAIN}")];
    lines.push("# normalized fixture-root prefix: temporary source and storage paths".into());
    for (field, reason) in TIMINGS {
        lines.push(format!("# normalized {field}: {reason}"));
    }
    lines.push("# normalized /structuredContent/plan/snapshot_generation: per-process checkout and vector publication identity".into());
    capture_case(&base, false, &mut lines, &mut raw_lines);
    capture_case(&base, true, &mut lines, &mut raw_lines);
    if let Some(output) = std::env::var_os("AFT_MAIN_PARITY_RAW") {
        std::fs::write(output, raw_lines.join("\n") + "\n").unwrap();
    }
    let recording = lines.join("\n") + "\n";
    if let Some(output) = std::env::var_os("AFT_MAIN_PARITY_CAPTURE") {
        std::fs::write(output, recording).unwrap();
        return;
    }
    let path = crate::test_support::manifest_dir()
        .join("../aft/tests/fixtures/grammars/main-1c6563b4-responses.golden");
    let expected = std::fs::read_to_string(path).unwrap();
    assert!(expected.starts_with(&format!("# main_commit={MAIN}\n")));
    if let Some(output) = std::env::var_os("AFT_MAIN_PARITY_ACTUAL") {
        std::fs::write(output, &recording).unwrap();
    }
    let differences = compare_recordings(&expected, &recording);
    assert!(differences.is_empty(), "main-pinned response bytes differ in {differences:?}; do not rewrite the recording from the current checkout");
}

fn align_callback_counts_as_set(expected: &Value, actual: &mut Value) {
    const POINTER: &str = "/structuredContent/plan/callback_counts";
    const REASON: &str = "main's iteration order is nondeterministic";
    let Some(reference) = expected.pointer(POINTER) else {
        return;
    };
    let found = actual
        .pointer_mut(POINTER)
        .expect("callback_counts missing");
    let reference_fields = reference.as_object().unwrap();
    let found_fields = found.as_object().expect("callback_counts is not an object");
    assert_eq!(
        reference_fields.len(),
        found_fields.len(),
        "callback_counts keys differ: {REASON}"
    );
    for (name, count) in reference_fields {
        let actual_count = found_fields.get(name).expect("callback_counts key missing");
        assert!(count.is_u64() && actual_count.is_u64());
        assert_eq!(
            count.to_string(),
            actual_count.to_string(),
            "callback_counts values differ: {REASON}"
        );
    }
    // Only this map's key order is a set comparison. Every enclosing key,
    // result row and text byte, including grep order, remains strict.
    *found = reference.clone();
}

fn align_intended_deltas(name: &str, reference: &Value, found: &mut Value) {
    // Inspect no longer counts the extra line at EOF (3dfbb334a8).
    const LOC: &str = "/summary/metrics/loc";
    let (pointer, before, after) = match name {
        "clean/inspect-dead-code" => (LOC, 10, 7),
        "invalid-utf8/inspect-dead-code" => (LOC, 14, 10),
        // Callers no longer scans an empty parse for invalid UTF-8 (fcf16967fe).
        "invalid-utf8/callers" => ("/scanned_files", 3, 2),
        _ => return,
    };
    assert_eq!(reference.pointer(pointer), Some(&json!(before)), "{name}");
    let actual = found
        .pointer_mut(pointer)
        .expect("expected field to compare");
    assert_eq!(*actual, json!(after), "{name}");
    *actual = json!(before);
}

fn compare_recordings(expected: &str, actual: &str) -> Vec<String> {
    assert_eq!(expected.lines().count(), actual.lines().count());
    expected
        .lines()
        .zip(actual.lines())
        .filter_map(|(expected, actual)| {
            if expected.starts_with('#') || actual.starts_with('#') {
                return (expected != actual).then(|| format!("header: {expected}"));
            }
            let (reference_name, reference_response) = expected.split_once('\t').unwrap();
            let (name, response) = actual.split_once('\t').unwrap();
            assert_eq!(name, reference_name);
            let reference: Value = serde_json::from_str(reference_response).unwrap();
            let mut found: Value = serde_json::from_str(response).unwrap();
            align_callback_counts_as_set(&reference, &mut found);
            align_intended_deltas(name, &reference, &mut found);
            (serde_json::to_string(&found).unwrap() != reference_response).then(|| name.to_string())
        })
        .collect()
}

#[test]
fn luau_s4_main_parity_preserves_grep_order_text_and_outer_key_order() {
    for (expected, actual) in [
        (
            "case\t{\"matches\":[1,2],\"text\":\"first then second\"}",
            "case\t{\"matches\":[2,1],\"text\":\"first then second\"}",
        ),
        (
            "case\t{\"text\":\"first then second\"}",
            "case\t{\"text\":\"second then first\"}",
        ),
        ("case\t{\"a\":1,\"b\":2}", "case\t{\"b\":2,\"a\":1}"),
    ] {
        assert_eq!(compare_recordings(expected, actual), vec!["case"]);
    }
}

#[test]
#[should_panic(expected = "callback_counts values differ")]
fn luau_s4_main_parity_rejects_changed_callback_counts() {
    let expected = json!({"structuredContent":{"plan":{"callback_counts":{"lexical":1}}}});
    let mut actual = json!({"structuredContent":{"plan":{"callback_counts":{"lexical":2}}}});
    align_callback_counts_as_set(&expected, &mut actual);
}

#[test]
#[should_panic(expected = "callback_counts keys differ")]
fn luau_s4_main_parity_rejects_missing_callback_keys() {
    let expected = json!({"structuredContent":{"plan":{"callback_counts":{"lexical":1}}}});
    let mut actual = json!({"structuredContent":{"plan":{"callback_counts":{}}}});
    align_callback_counts_as_set(&expected, &mut actual);
}

#[cfg(test)]
use aft_engine::test_bridge::display_data::DataCheckoutTestExt as _;
