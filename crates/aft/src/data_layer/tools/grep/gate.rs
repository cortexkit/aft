//! The grep and glob gate: every request answered by today's path and by
//! the data layer, on one real checkout, compared reply by reply.
//!
//! It copies a checkout (the files its ignore rules admit, with their
//! modification times), builds today's trigram index and the data layer
//! over the copy, and sends every case of a grep corpus and a glob corpus
//! through the real tool handlers twice: once with the data layer ignored,
//! as a process without it answers, and once reading it. It compares what
//! the agent is shown (the reply text, the metadata the plugins show, and
//! the text `subc_format::format_response` renders from the reply, or the
//! error) and reports every difference, then makes a few edits, records
//! them in both, and replays both corpora again.
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
//! It needs an optimized build with debug assertions on (the library's
//! tests use items that exist only with them), in a profile of its own so
//! it reuses no other profile's artifacts:
//!
//! ```text
//! AFT_DATA_LAYER=1 AFT_GATE_ROOT=<checkout> AFT_GATE_REPORT=<report.json> \
//!   cargo test --config 'profile.gate.inherits="release"' \
//!   --config 'profile.gate.lto=false' --config 'profile.gate.codegen-units=16' \
//!   --config 'profile.gate.debug-assertions=true' --config 'profile.gate.strip=false' \
//!   --config 'profile.gate.debug=0' --config 'profile.gate.split-debuginfo="off"' \
//!   --profile gate -p agent-file-tools --lib -- --ignored --nocapture \
//!   --exact data_layer::tools::grep::gate::grep_and_glob_answer_alike_from_the_data_layer
//! ```
//!
//! `AFT_GATE_LIMIT` keeps the first N grep cases; `AFT_GATE_CASES=<file>`
//! replays a JSON list of grep parameters instead of the corpus;
//! `AFT_GATE_ALLOW_DIFFS=1` reports differences without failing on them.

use std::collections::{BTreeMap, BTreeSet};
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::test_hooks;
use crate::context::AppContext;
use crate::data_layer::checkout::PlaneStatus;
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
    /// Cases whose shown reply differed, by class: what explains the
    /// difference, with the evidence for it.
    differences: Vec<Value>,
    classes: BTreeMap<&'static str, usize>,
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
            "by_class": self.classes,
            "hidden_field_differences": self.field_differences,
            "data_layer_answered": self.data_layer_answers,
            "today_read_data_layer": self.today_answers_from_data_layer,
        })
    }

    fn unexplained(&self) -> usize {
        self.classes
            .iter()
            .filter(|(class, _)| !ACCEPTED.contains(class))
            .map(|(_, count)| count)
            .sum()
    }
}

/// A published path deleted on disk with no change recorded: a reply says
/// it was not on disk only when the path was among the candidates it read,
/// and the two paths read different candidates. Its matches are the same.
/// A reply that discloses the stale index says it is not complete; the
/// data layer may disclose what today's reply does not, but never claims a
/// complete answer where today's reply discloses a stale index.
const STALE_PATH: &str = "stale path disclosed only when it is a candidate";
/// The data layer's reply claims a complete answer where today's reply
/// discloses that the index is out of date: not a wrong match, but an
/// answer that hides what it could not see.
const CLAIMS_COMPLETE: &str =
    "the data layer claims a complete answer where today's reply discloses a stale index";
/// Today's planner merges the posting filters of one trigram across the
/// alternatives of a pattern, so an alternative that is a prefix of
/// another inherits its next-character filter, and files holding only the
/// shorter one are never read. The data layer's answer equals a full scan.
const FILTER_MERGE: &str = "today's alternation filter merge drops candidates";
/// Both replies stopped at the most matches grep keeps for one request, so
/// their "(capped)" counts are floors that depend on which candidate files
/// each read before stopping. Every printed row is the same, and without
/// that limit both paths find exactly what a full scan finds.
const CAPPED_FLOOR: &str = "capped count is a floor that depends on the files read";
/// Differences explained and accepted.
const ACCEPTED: &[&str] = &[STALE_PATH, FILTER_MERGE, CAPPED_FLOOR];
/// Today misses matches a full scan finds, in a pattern without
/// alternatives, or the data layer's answer is not a full scan's.
const TODAY_MISSES_OTHER: &str = "today misses matches a full scan finds (no alternation)";
/// A difference that did not repeat when both calls ran again.
const DID_NOT_REPEAT: &str = "did not repeat";
const UNEXPLAINED: &str = "unexplained";

/// Full texts are kept for this many differences of each class.
const EXAMPLES_PER_CLASS: usize = 30;

fn replay(
    ctx: &AppContext,
    root: &Path,
    oracle: &mut Option<Oracle>,
    phase: &str,
    cases: &[Case],
    timings: &mut Timings,
) -> Replay {
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
        let (mut class, evidence) = classify(ctx, root, oracle, case, &shown_today, &shown_data);
        // Repeat an unexplained difference, and the first few of each
        // class, twice: one that does not repeat comes from timing (a time
        // budget), not from reading the data layer instead of today's index.
        let example = replay.classes.get(class).copied().unwrap_or(0) < EXAMPLES_PER_CLASS;
        let stable = (example || !ACCEPTED.contains(&class)).then(|| {
            (0..2).all(|_| {
                visible(case.tool, &call(ctx, case, true).0) == shown_today
                    && visible(case.tool, &call(ctx, case, false).0) == shown_data
            })
        });
        if stable == Some(false) && !ACCEPTED.contains(&class) {
            class = DID_NOT_REPEAT;
        }
        *replay.classes.entry(class).or_default() += 1;
        let mut difference = json!({
            "phase": phase,
            "tool": case.tool,
            "scope": case.scope,
            "params": serde_json::from_str::<Value>(&case.params).unwrap(),
            "class": class,
            "stable": stable,
            "first_difference": first_difference(&shown_today, &shown_data),
            "evidence": evidence,
        });
        // An unexplained difference keeps both replies whole, so the report
        // shows every line that differs.
        if !ACCEPTED.contains(&class) {
            difference["today"] = shown_today;
            difference["data"] = shown_data;
        } else if example {
            difference["today"] = clip(shown_today);
            difference["data"] = clip(shown_data);
        }
        replay.differences.push(difference);
    }
    replay
}

/// `shown` without the note a grep reply carries when indexed files it read
/// were gone. `complete` stays, so the replies' claims are compared.
fn without_missing_note(shown: &Value) -> Value {
    let note = regex::Regex::new(
        r"\n\n\(\d+ indexed file\(s\) were not on disk in this checkout; the index is out of date, so matches in files it has not seen may be missing\.\)",
    )
    .unwrap();
    let mut shown = shown.clone();
    for key in ["text", "rendered"] {
        if let Some(text) = shown[key].as_str() {
            shown[key] = json!(note.replace_all(text, "").into_owned());
        }
    }
    shown
}

/// Whether a grep difference comes from a stale path one reply disclosed:
/// [`STALE_PATH`] when the replies are the same apart from that note (and
/// `complete`, when only the data layer's reply disclosed it), or
/// [`CLAIMS_COMPLETE`] when they are the same apart from the note and the
/// data layer's reply claims `complete: true` where today's discloses the
/// stale index. `None` when neither reply carries the note, or they differ
/// in more.
fn stale_path_class(today: &Value, data: &Value) -> Option<(&'static str, Value)> {
    let noted = |shown: &Value| {
        shown["text"]
            .as_str()
            .is_some_and(|text| text.contains("indexed file(s) were not on disk in this checkout"))
    };
    if !noted(today) && !noted(data) {
        return None;
    }
    let (mut today_shown, data_shown) = (without_missing_note(today), without_missing_note(data));
    // Only the data layer disclosed the stale index, so only its reply says
    // it is not complete: the more honest answer.
    let only_data_noted = noted(data) && !noted(today);
    if only_data_noted && data["complete"] == json!(false) {
        today_shown["complete"] = json!(false);
    }
    if today_shown == data_shown {
        return Some((
            STALE_PATH,
            json!({"same_apart_from": if only_data_noted {
                "the not-on-disk note, and complete, which only the data layer's note withdrew"
            } else {
                "the not-on-disk note"
            }}),
        ));
    }
    today_shown["complete"] = data_shown["complete"].clone();
    (noted(today) && data["complete"] == json!(true) && today_shown == data_shown).then(|| {
        (
            CLAIMS_COMPLETE,
            json!({"today_complete": today["complete"], "data_complete": data["complete"]}),
        )
    })
}

/// What explains one difference, with its evidence.
fn classify(
    ctx: &AppContext,
    root: &Path,
    oracle: &mut Option<Oracle>,
    case: &Case,
    today: &Value,
    data: &Value,
) -> (&'static str, Value) {
    if case.tool == "grep" {
        if let Some(found) = stale_path_class(today, data) {
            return found;
        }
    }
    let Some((pattern, filters, scope)) = grep_query(root, case) else {
        return (UNEXPLAINED, Value::Null);
    };
    let lines = |today: bool| engine_lines(ctx, &pattern, &filters, &scope, today);
    let (Some(today_lines), Some(data_lines)) = (lines(true), lines(false)) else {
        return (UNEXPLAINED, json!({"engine": "a search failed"}));
    };
    let full = oracle
        .get_or_insert_with(|| Oracle::read(root))
        .lines(&pattern, &filters, &scope);
    let missed = full.difference(&today_lines).collect::<Vec<_>>();
    let query = crate::search_index::decompose_grep_pattern(&pattern);
    let evidence = json!({
        "full_scan_lines": full.len(),
        "today_lines": today_lines.len(),
        "data_lines": data_lines.len(),
        "today_extra": today_lines.difference(&full).take(5).map(show_line).collect::<Vec<_>>(),
        "today_missed": missed.iter().take(5).map(|line| show_line(line)).collect::<Vec<_>>(),
        "data_not_full_scan": data_lines.symmetric_difference(&full).take(5).map(show_line).collect::<Vec<_>>(),
        "today_query": clip_text(&format!("{query:?}"), 2000),
    });
    let today_subset = today_lines.is_subset(&full) && !missed.is_empty();
    let class = if data_lines != full {
        UNEXPLAINED
    } else if today_lines == full {
        // The same lines without the cap: the replies may differ only in
        // the floor their capped count gives.
        if capped_floors_alone(today, data) {
            CAPPED_FLOOR
        } else {
            UNEXPLAINED
        }
    } else if !today_subset {
        UNEXPLAINED
    } else if query.or_groups.is_empty() {
        TODAY_MISSES_OTHER
    } else {
        FILTER_MERGE
    };
    let mut evidence = evidence;
    if class == CAPPED_FLOOR {
        evidence["today_total"] = today["total_matches"].clone();
        evidence["data_total"] = data["total_matches"].clone();
    }
    (class, evidence)
}

/// Whether two capped grep replies differ only in their capped count: each
/// reply's own `total_matches` where its footer ("Found N matches …") and
/// its list trailer's `≥N` lower bound print it, and `total_matches`
/// itself, with every row the same.
fn capped_floors_alone(today: &Value, data: &Value) -> bool {
    let normalized = |shown: &Value| {
        let mut shown = shown.clone();
        let total = shown["total_matches"].as_u64();
        for key in ["text", "rendered"] {
            if let (Some(text), Some(total)) = (shown[key].as_str(), total) {
                let text = text
                    .replace(&format!("Found {total} match"), "Found # match")
                    .replace(&format!("≥{total}"), "≥#");
                shown[key] = json!(text);
            }
        }
        if let Some(object) = shown.as_object_mut() {
            object.remove("total_matches");
        }
        shown
    };
    today["truncated"] == json!(true)
        && data["truncated"] == json!(true)
        && normalized(today) == normalized(data)
}

fn show_line((path, line): &(PathBuf, u32)) -> String {
    format!("{}:{line}", path.display())
}

/// A grep case as the engine takes it: its compiled pattern, its include
/// and exclude filters and its folder. `None` when it does not compile or
/// names a file, which neither path searches through an index.
fn grep_query(
    root: &Path,
    case: &Case,
) -> Option<(
    crate::pattern_compile::CompiledPattern,
    crate::search_index::PathFilters,
    PathBuf,
)> {
    if case.tool != "grep" {
        return None;
    }
    let params: Value = serde_json::from_str(&case.params).ok()?;
    let strings = |key: &str| -> Vec<String> {
        params[key]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|value| value.as_str().map(str::to_string))
            .collect()
    };
    let case_insensitive = params["case_sensitive"] == json!(false);
    let crate::pattern_compile::CompileResult::Ok(pattern) = crate::pattern_compile::compile(
        params["pattern"].as_str()?,
        crate::pattern_compile::CompileOpts {
            case_insensitive,
            ..crate::pattern_compile::CompileOpts::default()
        },
    ) else {
        return None;
    };
    let filters =
        crate::search_index::build_path_filters(&strings("include"), &strings("exclude")).ok()?;
    let scope = match params["path"].as_str() {
        None => root.to_path_buf(),
        Some(path) if Path::new(path).is_absolute() => PathBuf::from(path),
        Some(path) => root.join(path),
    };
    let scope = std::fs::canonicalize(scope).ok()?;
    (!scope.is_file()).then_some((pattern, filters, scope))
}

/// Every matching line one path's index finds, with no result cap. The
/// scan keeps lines in arrival order: a path-ordered scan keeps only the
/// rows its row budget allows per file, which is all a capped reply needs
/// but not every line.
fn engine_lines(
    ctx: &AppContext,
    pattern: &crate::pattern_compile::CompiledPattern,
    filters: &crate::search_index::PathFilters,
    scope: &Path,
    today: bool,
) -> Option<BTreeSet<(PathBuf, u32)>> {
    let snapshot = if today {
        ctx.search_index().read().ok()?.as_ref()?.snapshot()
    } else {
        super::ViewScope::of(ctx.data_view()?, scope)?
            .candidates(pattern)
            .ok()?
    };
    let query = crate::search_index::decompose_grep_pattern(pattern);
    let (result, _) = snapshot
        .search_grep_profiled_with_filters_and_query_in_order(
            pattern,
            &query,
            filters,
            scope,
            usize::MAX / 4,
            None,
            crate::search_index::GrepScanOrder::Arrival,
        )
        .ok()?;
    (!result.scan_deadline_reached).then(|| {
        result
            .matches
            .into_iter()
            .map(|found| (found.file, found.line))
            .collect()
    })
}

/// A full scan: every file both indexes hold, read and matched line by
/// line, with no index or planner involved.
struct Oracle {
    root: PathBuf,
    files: Vec<(PathBuf, Vec<u8>)>,
}

impl Oracle {
    /// Reads every file of the checkout at `root` that today's index walk
    /// admits: the ignore rules (`.aftignore` files included), hidden files
    /// included, no symlinks, and none under `.git` or the folders that walk
    /// always skips. The folder the gate creates without telling either
    /// index is left out too.
    fn read(root: &Path) -> Self {
        let unrecorded = root.join("gate_unrecorded_dir");
        let walk = ignore::WalkBuilder::new(root)
            .hidden(false)
            .require_git(false)
            .add_custom_ignore_filename(".aftignore")
            .filter_entry(move |entry| {
                let name = entry.file_name().to_string_lossy();
                let is_dir = entry.file_type().is_some_and(|kind| kind.is_dir());
                name != ".git"
                    && entry.path() != unrecorded
                    && !(is_dir
                        && entry.depth() > 0
                        && crate::grep_executor::is_fallback_skipped_directory_name(&name))
            })
            .build();
        let files = walk
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()))
            .filter_map(|entry| {
                let bytes = std::fs::read(entry.path()).ok()?;
                Some((entry.into_path(), bytes))
            })
            .collect();
        Self {
            root: root.to_path_buf(),
            files,
        }
    }

    fn lines(
        &self,
        pattern: &crate::pattern_compile::CompiledPattern,
        filters: &crate::search_index::PathFilters,
        scope: &Path,
    ) -> BTreeSet<(PathBuf, u32)> {
        use crate::pattern_compile::CompiledPattern;
        let mut found = BTreeSet::new();
        for (path, bytes) in &self.files {
            if !path.starts_with(scope)
                || !filters.matches(&self.root, path)
                || crate::search_index::is_binary_bytes(bytes)
            {
                continue;
            }
            for (index, line) in bytes.split(|byte| *byte == b'\n').enumerate() {
                let hit = match pattern {
                    CompiledPattern::Literal(literal) => {
                        let line = if literal.case_insensitive_ascii {
                            line.to_ascii_lowercase()
                        } else {
                            line.to_vec()
                        };
                        memchr::memmem::find(&line, &literal.needle).is_some()
                    }
                    CompiledPattern::Regex { compiled, .. } => compiled.is_match(line),
                };
                if hit {
                    found.insert((path.clone(), index as u32 + 1));
                }
            }
        }
        found
    }
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
            value[key] = json!(clip_text(text, 6000));
        }
    }
    value
}

fn clip_text(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…[{} bytes]", &text[..end], text.len())
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
        if index % 20 == 0 {
            cases.insert(Case::new(
                "grep",
                variant("case_sensitive", json!(false)),
                root,
            ));
        }
        match index % 40 {
            1 | 21 => {
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
        if index % 50 == 4 {
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
            changes.push(path.as_bytes().to_vec());
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
            changes.push(path.as_bytes().to_vec());
            removed.push(absolute);
            done.push(format!("deleted: {path}"));
        }
    }
    if std::fs::remove_file(root.join("crates/aft/src/commands/outline.rs")).is_ok() {
        done.push("deleted, recorded in neither index: crates/aft/src/commands/outline.rs".into());
    }

    // Both indexes learn of the recorded edits once they are on disk.
    ctx.data_checkout()
        .expect("the data layer is bound")
        .checkout()
        .record_paths(&changes)
        .expect("the data layer records the edits");
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

    let (mut grep_cases, distinct_patterns) = match std::env::var_os("AFT_GATE_CASES") {
        // A list of grep parameters to replay instead of the corpus, such as
        // the cases an earlier run found different. A path starting with
        // `{root}` names a path under this run's copy of the checkout.
        Some(list) => {
            let list: Vec<Value> = serde_json::from_slice(&std::fs::read(list).unwrap()).unwrap();
            let cases = list
                .into_iter()
                .map(|mut params| {
                    if let Some(path) = params["path"].as_str() {
                        if let Some(rest) = path.strip_prefix("{root}") {
                            params["path"] = json!(format!("{}{rest}", root.display()));
                        }
                    }
                    Case::new("grep", params, &root)
                })
                .collect::<BTreeSet<_>>();
            let patterns = cases.len();
            (cases.into_iter().collect::<Vec<_>>(), patterns)
        }
        None => grep_corpus(&root, &corpus),
    };
    grep_cases.truncate(limit);
    let glob_cases = glob_corpus(&root);
    // An empty copy or corpus would compare nothing and pass.
    assert!(
        copied > 0 && data_files > 0 && today_files > 0,
        "nothing to compare: {copied} files copied, {data_files} in the data layer, {today_files} in today's index"
    );
    assert!(
        !grep_cases.is_empty() && !glob_cases.is_empty(),
        "nothing to compare: {} grep cases, {} glob cases",
        grep_cases.len(),
        glob_cases.len()
    );
    eprintln!(
        "gate: {} grep cases from {distinct_patterns} patterns, {} glob cases",
        grep_cases.len(),
        glob_cases.len()
    );
    let mut timings = Timings::default();
    let mut oracle = None;
    let mut replays = BTreeMap::new();
    let mut run = |name, phase, cases: &[Case], oracle: &mut Option<Oracle>| {
        let replay = replay(&ctx, &root, oracle, phase, cases, &mut timings);
        replays.insert(name, replay);
    };
    run("1 grep", "published", &grep_cases, &mut oracle);
    run("2 glob", "published", &glob_cases, &mut oracle);
    let (edits, edit_cases) = edit_checkout(&ctx, &root);
    // The full scan reads the edited files again.
    let mut oracle = None;
    run("3 grep after edits", "edited", &grep_cases, &mut oracle);
    run("4 glob after edits", "edited", &glob_cases, &mut oracle);
    run("5 edited paths", "edited", &edit_cases, &mut oracle);

    let differences = replays
        .values()
        .flat_map(|replay| replay.differences.iter().cloned())
        .collect::<Vec<_>>();
    let capped = differences
        .iter()
        .filter(|difference| difference["class"] == CAPPED_FLOOR)
        .map(|difference| {
            json!({
                "phase": difference["phase"],
                "params": difference["params"],
                "today_total": difference["evidence"]["today_total"],
                "data_total": difference["evidence"]["data_total"],
                "full_scan_lines": difference["evidence"]["full_scan_lines"],
            })
        })
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
        "capped_cases": capped,
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
    let unexplained = replays.values().map(Replay::unexplained).sum::<usize>();
    assert!(
        allow_differences || unexplained == 0,
        "{unexplained} replies differ for no accepted reason; see the report"
    );
}

fn count_by_scope(cases: &[Case]) -> BTreeMap<&'static str, usize> {
    let mut counts = BTreeMap::new();
    for case in cases {
        *counts.entry(case.scope).or_default() += 1;
    }
    counts
}

/// A stale path disclosed by one reply is accepted only when the data
/// layer's reply does not claim a complete answer that today's reply
/// withdraws.
#[test]
fn a_stale_path_class_never_accepts_a_data_layer_claim_of_complete() {
    let note = "\n\n(1 indexed file(s) were not on disk in this checkout; the index is out of date, so matches in files it has not seen may be missing.)";
    let reply = |noted: bool, complete: bool| {
        let text = format!("src/a.rs:1: hit{}", if noted { note } else { "" });
        json!({"text": text, "rendered": text, "complete": complete, "matches": 1})
    };
    let class = |today: &Value, data: &Value| stale_path_class(today, data).map(|(class, _)| class);
    // Today discloses the stale index and so is not complete; the data
    // layer's reply, without the note, must not claim that it is.
    assert_eq!(
        class(&reply(true, false), &reply(false, true)),
        Some(CLAIMS_COMPLETE)
    );
    assert_eq!(
        class(&reply(true, false), &reply(false, false)),
        Some(STALE_PATH)
    );
    // The data layer disclosing what today's reply does not is accepted.
    assert_eq!(
        class(&reply(false, true), &reply(true, false)),
        Some(STALE_PATH)
    );
    assert_eq!(
        class(&reply(true, false), &reply(true, false)),
        Some(STALE_PATH)
    );
    // Neither reply disclosed anything: another class explains it.
    assert_eq!(class(&reply(false, true), &reply(false, false)), None);
    // Matches that differ are not explained by the note.
    let mut other = reply(false, false);
    other["matches"] = json!(2);
    assert_eq!(class(&reply(true, false), &other), None);
}
