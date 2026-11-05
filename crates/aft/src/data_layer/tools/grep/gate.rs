//! The grep and glob gate: every request answered by today's path and by
//! the data layer, on one real checkout, compared reply by reply.
//!
//! It copies a checkout (the files its ignore rules admit, with their
//! modification times), builds today's trigram index and the data layer
//! over the copy, and sends every case of a grep corpus and a glob corpus
//! through the real tool handlers twice: once with the data layer ignored,
//! as a process without it answers, and once reading it. It compares what
//! the agent is shown (the reply text, the metadata the plugins show, and
//! the subc rendering, or the error) and reports every difference, then
//! makes a few edits, records them in both, and replays both corpora again.
//!
//! The grep corpus is real: each distinct pattern of
//! `aft-store/examples/data/grep-patterns-14d.jsonl.gz` with its path
//! argument mapped onto this checkout (the longest prefix of the path that
//! exists here; a path from another repository keeps its shape: the
//! checkout itself, a folder or a file). The corpus records no other
//! flags, so a fixed share of cases also runs case-insensitive, with
//! include or exclude globs, or at an offset. The glob corpus crosses the
//! common glob shapes with the common scopes.
//!
//! ```text
//! AFT_DATA_LAYER=1 AFT_GATE_ROOT=<checkout> AFT_GATE_REPORT=<report.json> \
//!   cargo test --profile ci-storm -p agent-file-tools --lib -- --ignored --nocapture \
//!   --exact data_layer::tools::grep::gate::grep_and_glob_answer_alike_from_the_data_layer
//! ```
//!
//! `AFT_GATE_LIMIT` keeps the first N grep cases, `AFT_GATE_ALLOW_DIFFS=1`
//! reports differences without failing.

use std::collections::{BTreeMap, BTreeSet};
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::test_hooks;
use crate::context::AppContext;
use crate::data_layer::checkout::{PathChange, PlaneStatus};
use crate::protocol::{RawRequest, Response};
use crate::search_index::SearchIndex;

/// One request of a corpus.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Case {
    tool: &'static str,
    /// The request's parameters, as JSON text so cases sort and dedupe.
    params: String,
    /// What the request's path names: `checkout` (no path, or the
    /// checkout itself), `directory` or `file`.
    scope: &'static str,
}

impl Case {
    fn new(tool: &'static str, params: Value, root: &Path) -> Self {
        let scope = match params.get("path").and_then(Value::as_str) {
            None => "checkout",
            Some(path) => {
                let absolute = root.join(path);
                if absolute == root || path == "." {
                    "checkout"
                } else if absolute.is_file() {
                    "file"
                } else {
                    "directory"
                }
            }
        };
        Self {
            tool,
            params: params.to_string(),
            scope,
        }
    }

    fn request(&self) -> RawRequest {
        let mut request: Value = serde_json::from_str(&self.params).unwrap();
        request["id"] = json!("gate");
        request["command"] = json!(self.tool);
        serde_json::from_value(request).unwrap()
    }
}

/// What the agent is shown for a reply.
fn visible(tool: &str, response: &Response) -> Value {
    let data = &response.data;
    if !response.success {
        return json!({"code": data["code"], "message": data["message"]});
    }
    let mut seen = json!({
        "text": data["text"],
        "complete": data["complete"],
        "truncated": data["truncated"],
        "rendered": crate::subc_format::format_response(tool, response, false),
    });
    if tool == "grep" {
        seen["matches"] = json!(data["matches"].as_array().map_or(0, Vec::len));
        seen["total_matches"] = data["total_matches"].clone();
        seen["rendered_matches"] = data["rendered_matches"].clone();
    } else {
        seen["count"] = json!(data["files"].as_array().map_or(0, Vec::len));
        seen["total"] = data["total"].clone();
    }
    seen
}

/// Reply fields no agent is shown, compared for the report only.
const GREP_FIELDS: &[&str] = &[
    "files_searched",
    "files_read_directly",
    "files_with_matches",
    "index_status",
    "fallback",
    "no_files_matched_scope",
    "missing_on_disk_dropped",
    "display_matches",
    "counted_matches",
    "counted_files",
    "next_offset",
    "scan_deadline_reached",
    "index_read_gap",
    "matches",
];
const GLOB_FIELDS: &[&str] = &[
    "files",
    "no_files_matched_scope",
    "fallback",
    "missing_on_disk_dropped",
    "total_lower_bound",
    "files_list_envelope",
    "walk_truncated",
];

/// One call: its reply and how long it took.
fn call(ctx: &AppContext, case: &Case, today: bool) -> (Response, f64) {
    let request = case.request();
    let started = Instant::now();
    let response = if today {
        test_hooks::without_data_layer(|| dispatch(ctx, case.tool, &request))
    } else {
        dispatch(ctx, case.tool, &request)
    };
    (response, started.elapsed().as_secs_f64() * 1000.0)
}

fn dispatch(ctx: &AppContext, tool: &str, request: &RawRequest) -> Response {
    match tool {
        "grep" => crate::commands::grep::handle_grep(request, ctx),
        _ => crate::commands::glob::handle_glob(request, ctx),
    }
}

#[derive(Default)]
struct Timings(BTreeMap<String, Vec<f64>>);

impl Timings {
    fn add(&mut self, key: String, ms: f64) {
        self.0.entry(key).or_default().push(ms);
    }

    fn summary(&self) -> Value {
        let mut out = serde_json::Map::new();
        for (key, values) in &self.0 {
            let mut sorted = values.clone();
            sorted.sort_by(f64::total_cmp);
            let at = |p: f64| sorted[((sorted.len() - 1) as f64 * p).round() as usize];
            let mean = sorted.iter().sum::<f64>() / sorted.len() as f64;
            out.insert(
                key.clone(),
                json!({
                    "calls": sorted.len(),
                    "mean_ms": round(mean),
                    "p50_ms": round(at(0.5)),
                    "p90_ms": round(at(0.9)),
                    "p99_ms": round(at(0.99)),
                    "max_ms": round(*sorted.last().unwrap()),
                    "total_s": round(sorted.iter().sum::<f64>() / 1000.0),
                }),
            );
        }
        Value::Object(out)
    }
}

fn round(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}

/// What one replay of a corpus found.
#[derive(Default)]
struct Replay {
    cases: usize,
    identical: usize,
    /// Cases whose shown reply differed, with both replies and whether
    /// repeating the case gave the same replies again.
    differences: Vec<Value>,
    /// Per hidden field, how many cases differed in it.
    field_differences: BTreeMap<String, usize>,
    /// Calls the data layer answered, and calls today's path answered that
    /// read it anyway (which must be none).
    data_layer_answers: usize,
    today_answers_from_data_layer: usize,
}

impl Replay {
    fn summary(&self) -> Value {
        json!({
            "cases": self.cases,
            "identical": self.identical,
            "different": self.differences.len(),
            "unstable": self.differences.iter().filter(|d| d["stable"] == false).count(),
            "hidden_field_differences": self.field_differences,
            "data_layer_answered": self.data_layer_answers,
            "today_read_data_layer": self.today_answers_from_data_layer,
        })
    }
}

fn replay(ctx: &AppContext, phase: &str, cases: &[Case], timings: &mut Timings) -> Replay {
    let mut replay = Replay::default();
    for (index, case) in cases.iter().enumerate() {
        // Alternate which path runs first, so neither always meets a
        // colder page cache.
        let today_first = index % 2 == 0;
        let run = |today: bool| {
            let before = test_hooks::answers();
            let (response, ms) = call(ctx, case, today);
            (response, ms, test_hooks::answers() - before)
        };
        let (first, second) = (run(today_first), run(!today_first));
        let ((today, today_ms, today_reads), (data, data_ms, data_reads)) = if today_first {
            (first, second)
        } else {
            (second, first)
        };
        replay.cases += 1;
        replay.data_layer_answers += usize::from(data_reads > 0);
        replay.today_answers_from_data_layer += usize::from(today_reads > 0);
        timings.add(
            format!("{phase} {} {} today", case.tool, case.scope),
            today_ms,
        );
        timings.add(
            format!("{phase} {} {} data", case.tool, case.scope),
            data_ms,
        );
        timings.add(format!("{phase} {} all today", case.tool), today_ms);
        timings.add(format!("{phase} {} all data", case.tool), data_ms);

        let fields = if case.tool == "grep" {
            GREP_FIELDS
        } else {
            GLOB_FIELDS
        };
        for field in fields {
            if today.data.get(*field) != data.data.get(*field) {
                *replay
                    .field_differences
                    .entry((*field).to_string())
                    .or_default() += 1;
            }
        }
        let (shown_today, shown_data) = (visible(case.tool, &today), visible(case.tool, &data));
        if shown_today == shown_data {
            replay.identical += 1;
            continue;
        }
        // Repeat both twice: a difference that does not repeat comes from
        // timing (a time budget), not from where the files came from.
        let stable = (0..2).all(|_| {
            visible(case.tool, &call(ctx, case, true).0) == shown_today
                && visible(case.tool, &call(ctx, case, false).0) == shown_data
        });
        replay.differences.push(json!({
            "phase": phase,
            "tool": case.tool,
            "scope": case.scope,
            "params": serde_json::from_str::<Value>(&case.params).unwrap(),
            "stable": stable,
            "first_difference": first_difference(&shown_today, &shown_data),
            "today": clip(shown_today),
            "data": clip(shown_data),
        }));
    }
    replay
}

/// The first line where the two shown texts differ, or the first field.
fn first_difference(today: &Value, data: &Value) -> Value {
    let text = |value: &Value| value["text"].as_str().unwrap_or_default().to_string();
    let (today_text, data_text) = (text(today), text(data));
    if today_text != data_text {
        let today_lines = today_text.lines().collect::<Vec<_>>();
        let data_lines = data_text.lines().collect::<Vec<_>>();
        let line = (0..today_lines.len().max(data_lines.len()))
            .find(|line| today_lines.get(*line) != data_lines.get(*line))
            .unwrap_or(0);
        return json!({
            "line": line + 1,
            "today": today_lines.get(line),
            "data": data_lines.get(line),
        });
    }
    let fields = today
        .as_object()
        .into_iter()
        .flat_map(|object| object.keys())
        .filter(|key| today[key.as_str()] != data[key.as_str()])
        .collect::<Vec<_>>();
    json!({ "fields": fields })
}

fn clip(mut value: Value) -> Value {
    for key in ["text", "rendered", "message"] {
        if let Some(text) = value[key].as_str() {
            if text.len() > 6000 {
                let mut end = 6000;
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                value[key] = json!(format!("{}…[{} bytes]", &text[..end], text.len()));
            }
        }
    }
    value
}

/// Copies the files `source`'s ignore rules admit into `dest`, with their
/// modification times, so both indexes see the files and the order a real
/// checkout gives.
fn copy_checkout(source: &Path, dest: &Path) -> usize {
    let mut copied = 0;
    let walk = ignore::WalkBuilder::new(source)
        .hidden(false)
        .require_git(false)
        .filter_entry(|entry| entry.file_name() != ".git")
        .build();
    for entry in walk {
        let entry = entry.unwrap();
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let relative = entry.path().strip_prefix(source).unwrap();
        let target = dest.join(relative);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::copy(entry.path(), &target).unwrap();
        let modified = entry.metadata().unwrap().modified().unwrap();
        filetime::set_file_mtime(&target, filetime::FileTime::from_system_time(modified)).unwrap();
        copied += 1;
    }
    copied
}

/// A corpus path mapped onto the checkout at `root`: the longest prefix of
/// it that exists here, spelled absolute when the corpus spelled it so.
/// A path with no such prefix names the checkout itself.
fn map_path(root: &Path, raw: &str) -> String {
    let raw = raw.trim();
    let absolute = raw.starts_with('/') || raw.starts_with('~');
    let parts = raw
        .split('/')
        .filter(|part| !part.is_empty() && *part != "." && *part != "~")
        .collect::<Vec<_>>();
    let start = if absolute {
        // A checkout under the user's projects folders starts after its
        // repository's folder; anywhere else, at the first part that is a
        // top-level entry here.
        match parts
            .iter()
            .position(|part| *part == "CortexKit" || *part == "CortexKit-scratch")
        {
            Some(projects) => projects + 2,
            None => parts
                .iter()
                .position(|part| root.join(part).exists())
                .unwrap_or(parts.len()),
        }
    } else {
        0
    };
    let mut mapped = PathBuf::new();
    for part in parts.iter().skip(start) {
        let next = mapped.join(part);
        if *part == ".." || !root.join(&next).exists() {
            break;
        }
        mapped = next;
    }
    match (absolute, mapped.as_os_str().is_empty()) {
        (true, _) => root.join(&mapped).display().to_string(),
        (false, true) => ".".to_string(),
        (false, false) => mapped.display().to_string(),
    }
}

fn grep_corpus(root: &Path, corpus: &Path) -> (Vec<Case>, usize) {
    let reader = std::io::BufReader::new(flate2::read::GzDecoder::new(
        std::fs::File::open(corpus).unwrap(),
    ));
    let mut patterns = BTreeSet::new();
    let mut requests = BTreeSet::new();
    for line in reader.lines() {
        let value: Value = serde_json::from_str(&line.unwrap()).unwrap();
        let Some(pattern) = value["pattern"].as_str() else {
            continue;
        };
        patterns.insert(pattern.to_string());
        let path = value["path"].as_str().map(|path| map_path(root, path));
        requests.insert((pattern.to_string(), path));
    }
    let mut cases = BTreeSet::new();
    for (index, (pattern, path)) in requests.into_iter().enumerate() {
        let mut params = json!({ "pattern": pattern });
        if let Some(path) = path {
            params["path"] = json!(path);
        }
        cases.insert(Case::new("grep", params.clone(), root));
        // Fixed shares of the corpus with the flags the corpus does not
        // record.
        let variant = |key: &str, value: Value| {
            let mut params = params.clone();
            params[key] = value;
            params
        };
        if index % 9 == 0 {
            cases.insert(Case::new(
                "grep",
                variant("case_sensitive", json!(false)),
                root,
            ));
        }
        match index % 10 {
            1 => {
                cases.insert(Case::new("grep", variant("include", json!(["*.rs"])), root));
            }
            2 => {
                let mut params = variant("include", json!(["**/*.{ts,tsx}"]));
                params["exclude"] = json!(["**/__tests__/**"]);
                cases.insert(Case::new("grep", params, root));
            }
            3 => {
                cases.insert(Case::new(
                    "grep",
                    variant("exclude", json!(["**/tests/**", "*.md"])),
                    root,
                ));
            }
            _ => {}
        }
        if index % 25 == 4 {
            cases.insert(Case::new("grep", variant("offset", json!(40)), root));
        }
    }
    (cases.into_iter().collect(), patterns.len())
}

const GLOB_PATTERNS: &[&str] = &[
    "**/*.rs",
    "*.rs",
    "**/*.ts",
    "**/*.tsx",
    "**/*.{ts,tsx}",
    "**/*.test.ts",
    "**/*.md",
    "*.md",
    "**/*.json",
    "*.json",
    "**/*.toml",
    "*.toml",
    "**/Cargo.toml",
    "**/package.json",
    "**/README.md",
    "**/mod.rs",
    "**/lib.rs",
    "**/main.rs",
    "**/index.ts",
    "**/*grep*",
    "**/*glob*",
    "**/*test*",
    "**/*[Tt]est*",
    "**/?.rs",
    "**/*.py",
    "**/*.sh",
    "**/*.yml",
    "**/*.yaml",
    "**/*.lock",
    "**/*.gz",
    "**/*.d.ts",
    "**/.gitignore",
    "*",
    "**/*",
    "**",
    "src/**/*.rs",
    "src/**/*.ts",
    "**/__tests__/**",
    "**/__tests__/**/*.test.ts",
    "**/tests/**/*.rs",
    ".github/workflows/*.yml",
    "crates/*/Cargo.toml",
    "packages/*/package.json",
    "crates/aft/src/**/*.rs",
    "crates/aft/src/commands/*.rs",
    "docs/**/*.md",
    "**/*.{rs,ts}",
    "**/data_layer/**",
    "**/{grep,glob}.rs",
    "Cargo.toml",
    "lib.rs",
    "crates/aft/src/lib.rs",
    "**/*.swift",
];

const SCOPES: &[Option<&str>] = &[
    None,
    Some("."),
    Some("crates"),
    Some("crates/aft"),
    Some("crates/aft/src"),
    Some("crates/aft/src/commands"),
    Some("crates/aft/src/data_layer"),
    Some("crates/aft-store"),
    Some("packages"),
    Some("packages/opencode-plugin/src"),
    Some("docs"),
    Some("scripts"),
    Some("benchmarks"),
    Some("tests"),
    Some("crates/aft/src/lib.rs"),
    Some("Cargo.toml"),
];

fn glob_corpus(root: &Path) -> Vec<Case> {
    let mut cases = BTreeSet::new();
    for scope in SCOPES {
        if scope.is_some_and(|scope| !root.join(scope).exists()) {
            continue;
        }
        for pattern in GLOB_PATTERNS {
            let mut params = json!({ "pattern": pattern });
            if let Some(scope) = scope {
                params["path"] = json!(scope);
            }
            cases.insert(Case::new("glob", params, root));
        }
    }
    // A few absolute spellings, as agents also send them.
    for scope in ["", "crates/aft/src"] {
        for pattern in ["**/*.rs", "*.md", "**/*"] {
            let path = root.join(scope).display().to_string();
            cases.insert(Case::new(
                "glob",
                json!({ "pattern": pattern, "path": path }),
                root,
            ));
        }
    }
    cases.into_iter().collect()
}

/// The edits made between the two replays, and the cases that look at
/// them directly.
fn edit_checkout(ctx: &AppContext, root: &Path) -> (Vec<String>, Vec<Case>) {
    let mut done = Vec::new();
    let mut changes = Vec::new();
    let mut updated = Vec::new();
    let mut removed = Vec::new();
    let mut write = |path: &str, bytes: Vec<u8>, what: &str, record: bool| {
        let absolute = root.join(path);
        std::fs::create_dir_all(absolute.parent().unwrap()).unwrap();
        std::fs::write(&absolute, &bytes).unwrap();
        if record {
            changes.push(PathChange::Written {
                path: path.as_bytes().to_vec(),
                bytes: Arc::from(bytes),
            });
            updated.push(absolute);
        }
        done.push(format!("{what}: {path}"));
    };
    let read = |path: &str| std::fs::read(root.join(path)).ok();

    if let Some(bytes) = read("crates/aft/src/commands/grep.rs") {
        let text = String::from_utf8_lossy(&bytes);
        let lines = text.lines().collect::<Vec<_>>();
        let mut edited = lines[..lines.len() / 2].join("\n");
        edited.push_str(
            "\n// data-layer gate edit: TODO FIXME pattern grep\nfn grep_gate_marker() -> usize { 42 }\n",
        );
        write(
            "crates/aft/src/commands/grep.rs",
            edited.into_bytes(),
            "edited (second half replaced)",
            true,
        );
    }
    if let Some(mut bytes) = read("README.md") {
        bytes.insert(0, 0);
        write("README.md", bytes, "made binary", true);
    }
    if let Some(bytes) = read("crates/aft/src/grep_executor.rs") {
        write(
            "crates/aft/src/commands/gate_created.rs",
            bytes,
            "created (a copy of grep_executor.rs)",
            true,
        );
    }
    let script = read("packages/opencode-plugin/src/tools/search.ts")
        .unwrap_or_else(|| b"export function grepGateMarker() { return 42; }\n".to_vec());
    write(
        "gate_new_dir/nested/created.ts",
        script,
        "created in a new folder",
        true,
    );
    write(
        "gate_new_dir/big.txt",
        "fn grep_gate_marker TODO alpha export\n"
            .repeat(32_000)
            .into_bytes(),
        "created over the indexing limit",
        true,
    );
    write(
        "gate_unrecorded_dir/file.rs",
        b"fn grep_gate_marker() {}\n".to_vec(),
        "created, recorded in neither index",
        false,
    );
    for path in [
        "crates/aft/src/commands/glob.rs",
        "crates/aft-store/examples/state_size.rs",
        "crates/aft-store/examples/text_gate.rs",
    ] {
        let absolute = root.join(path);
        if std::fs::remove_file(&absolute).is_ok() {
            changes.push(PathChange::Removed {
                path: path.as_bytes().to_vec(),
            });
            removed.push(absolute);
            done.push(format!("deleted: {path}"));
        }
    }
    if std::fs::remove_file(root.join("crates/aft/src/commands/outline.rs")).is_ok() {
        done.push("deleted, recorded in neither index: crates/aft/src/commands/outline.rs".into());
    }

    ctx.data_checkout()
        .expect("the data layer is bound")
        .checkout()
        .record_changes(&changes);
    {
        let mut index = ctx.search_index().write().unwrap();
        let index = index.as_mut().expect("today's index is installed");
        for path in &updated {
            index.update_file(path);
        }
        for path in &removed {
            index.remove_file(path);
        }
    }

    let scopes = [
        None,
        Some("gate_new_dir"),
        Some("gate_new_dir/nested"),
        Some("gate_unrecorded_dir"),
        Some("crates/aft/src/commands"),
        Some("crates/aft-store/examples"),
    ];
    let mut cases = BTreeSet::new();
    for scope in scopes {
        for pattern in ["grep_gate_marker", "fn ", "TODO", "export", "."] {
            let mut params = json!({ "pattern": pattern });
            if let Some(scope) = scope {
                params["path"] = json!(scope);
            }
            cases.insert(Case::new("grep", params, root));
        }
        for pattern in ["**/*", "*", "**/*.ts", "**/*.rs", "*.txt"] {
            let mut params = json!({ "pattern": pattern });
            if let Some(scope) = scope {
                params["path"] = json!(scope);
            }
            cases.insert(Case::new("glob", params, root));
        }
    }
    (done, cases.into_iter().collect())
}

#[test]
#[ignore = "replays real corpora over a real checkout; see the module docs"]
fn grep_and_glob_answer_alike_from_the_data_layer() {
    assert!(
        crate::data_layer::runtime::enabled(),
        "run the gate with AFT_DATA_LAYER=1"
    );
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let source =
        std::env::var_os("AFT_GATE_ROOT").map_or_else(|| manifest.join("../.."), PathBuf::from);
    let corpus = std::env::var_os("AFT_GATE_CORPUS").map_or_else(
        || manifest.join("../aft-store/examples/data/grep-patterns-14d.jsonl.gz"),
        PathBuf::from,
    );
    let limit = std::env::var("AFT_GATE_LIMIT")
        .ok()
        .map_or(usize::MAX, |limit| limit.parse().unwrap());
    let allow_differences = std::env::var_os("AFT_GATE_ALLOW_DIFFS").is_some_and(|v| v == "1");

    let work = tempfile::tempdir().unwrap();
    let root = work.path().join("checkout");
    let started = Instant::now();
    let copied = copy_checkout(&std::fs::canonicalize(&source).unwrap(), &root);
    let copy_ms = started.elapsed().as_secs_f64() * 1000.0;
    let root = std::fs::canonicalize(&root).unwrap();
    let ctx = AppContext::new(
        crate::context::default_language_provider_factory(),
        crate::config::Config {
            project_root: Some(root.clone()),
            storage_dir: Some(work.path().join("storage")),
            ..crate::config::Config::default()
        },
    );
    ctx.set_canonical_cache_root(root.clone());

    let started = Instant::now();
    let index = SearchIndex::build(&root);
    assert!(index.is_ready());
    let today_files = index.file_count();
    *ctx.search_index().write().unwrap() = Some(index);
    let today_build_ms = started.elapsed().as_secs_f64() * 1000.0;

    let started = Instant::now();
    assert!(
        ctx.data_checkout().is_some(),
        "the data layer binds the copy"
    );
    let deadline = Instant::now() + Duration::from_secs(1800);
    let view = loop {
        if let Some(view) = ctx.data_view() {
            if view.status().text == PlaneStatus::Ready {
                break view;
            }
        }
        assert!(Instant::now() < deadline, "the data layer never published");
        std::thread::sleep(Duration::from_millis(100));
    };
    let data_build_ms = started.elapsed().as_secs_f64() * 1000.0;
    let data_files = view.state().paths().len();
    drop(view);

    let (mut grep_cases, distinct_patterns) = grep_corpus(&root, &corpus);
    grep_cases.truncate(limit);
    let glob_cases = glob_corpus(&root);
    let mut timings = Timings::default();
    let mut replays = BTreeMap::new();
    replays.insert(
        "1 grep",
        replay(&ctx, "published", &grep_cases, &mut timings),
    );
    replays.insert(
        "2 glob",
        replay(&ctx, "published", &glob_cases, &mut timings),
    );
    let (edits, edit_cases) = edit_checkout(&ctx, &root);
    replays.insert(
        "3 grep after edits",
        replay(&ctx, "edited", &grep_cases, &mut timings),
    );
    replays.insert(
        "4 glob after edits",
        replay(&ctx, "edited", &glob_cases, &mut timings),
    );
    replays.insert(
        "5 edited paths",
        replay(&ctx, "edited", &edit_cases, &mut timings),
    );

    let differences = replays
        .values()
        .flat_map(|replay| replay.differences.iter().cloned())
        .collect::<Vec<_>>();
    let summary = json!({
        "checkout": {
            "source": source.display().to_string(),
            "files_copied": copied,
            "copy_ms": round(copy_ms),
            "today_index_files": today_files,
            "today_index_build_ms": round(today_build_ms),
            "data_layer_paths": data_files,
            "data_layer_build_ms": round(data_build_ms),
        },
        "grep_corpus": {
            "distinct_patterns": distinct_patterns,
            "cases": grep_cases.len(),
            "by_scope": count_by_scope(&grep_cases),
        },
        "glob_cases": glob_cases.len(),
        "edits": edits,
        "replays": replays.iter().map(|(name, replay)| (name.to_string(), replay.summary())).collect::<serde_json::Map<_, _>>(),
        "timings": timings.summary(),
    });
    println!("{}", serde_json::to_string_pretty(&summary).unwrap());
    if let Some(report) = std::env::var_os("AFT_GATE_REPORT") {
        let report = PathBuf::from(report);
        std::fs::create_dir_all(report.parent().unwrap()).unwrap();
        std::fs::write(
            &report,
            serde_json::to_string_pretty(&json!({
                "summary": summary,
                "differences": differences,
            }))
            .unwrap(),
        )
        .unwrap();
    }
    for replay in replays.values() {
        assert_eq!(
            replay.today_answers_from_data_layer, 0,
            "today's path read the data layer"
        );
    }
    assert!(
        replays.values().all(|replay| replay.data_layer_answers > 0),
        "a replay never read the data layer"
    );
    assert!(
        allow_differences || differences.is_empty(),
        "{} replies differ; see the report",
        differences.len()
    );
}

fn count_by_scope(cases: &[Case]) -> BTreeMap<&'static str, usize> {
    let mut counts = BTreeMap::new();
    for case in cases {
        *counts.entry(case.scope).or_default() += 1;
    }
    counts
}
