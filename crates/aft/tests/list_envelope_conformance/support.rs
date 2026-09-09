#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use aft::commands::callgraph_store_adapter::callgraph_surface::{
    build_callgraph_envelope, cap_items, hub_selector_activated, HUB_SUMMARY_LIMIT,
};
use aft::commands::callgraph_store_adapter::{
    StoreCallTreeNode, StoreCallerEntry, StoreCallerGroup, StoreCallersResult, StoreHubSummary,
    StoreImpactCaller, StoreImpactResult, HUB_SUMMARY_THRESHOLD,
};
use aft::commands::trace_to::trace::{build_trace_data_envelope, build_trace_to_envelope};
use aft::list_envelope::{derive_wire_key, ListEnvelope, Reason, Total, Unit};
use aft::list_surfaces::bash::{
    append_envelope_trailer, build_bash_output_envelope, produce_bash_reply_data,
};
use aft::list_surfaces::glob::build_glob_envelope;
use aft::list_surfaces::grep::build_grep_envelope_from_parts;
use aft::list_surfaces::inspect::build_inspect_envelope;
use aft::list_surfaces::outline::build_outline_files_envelope;
use aft::list_surfaces::search::{attach_search_envelope, build_search_envelope};
use aft::list_surfaces::{ExclusionEntry, ReasonKind, EXCLUSIONS, LIST_SURFACES};
use aft::ndjson_text::build_ndjson_text;
use aft::protocol::Response;
use aft::subc_format::{format_response_with_context, FormatContext, OutlineMode};
use serde_json::{json, Map, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReasonSpec {
    pub reason: Reason,
    pub kind: ReasonKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SurfaceSpec {
    pub command: &'static str,
    pub mode: &'static str,
    pub list_id: &'static str,
    pub unit: Unit,
    pub narrow: &'static [&'static str],
    pub reasons: &'static [ReasonSpec],
}

const CAP_SELECTING: &[ReasonSpec] = &[ReasonSpec {
    reason: Reason::Cap,
    kind: ReasonKind::Selecting,
}];
const DEPTH_BOUNDING: &[ReasonSpec] = &[ReasonSpec {
    reason: Reason::Depth,
    kind: ReasonKind::Bounding,
}];
const CAP_DEPTH: &[ReasonSpec] = &[
    ReasonSpec {
        reason: Reason::Cap,
        kind: ReasonKind::Selecting,
    },
    ReasonSpec {
        reason: Reason::Depth,
        kind: ReasonKind::Bounding,
    },
];
const DEPTH_BUDGET_CAP: &[ReasonSpec] = &[
    ReasonSpec {
        reason: Reason::Depth,
        kind: ReasonKind::Bounding,
    },
    ReasonSpec {
        reason: Reason::Budget,
        kind: ReasonKind::Bounding,
    },
    ReasonSpec {
        reason: Reason::Cap,
        kind: ReasonKind::Selecting,
    },
];
const BUDGET_CAP: &[ReasonSpec] = &[
    ReasonSpec {
        reason: Reason::Budget,
        kind: ReasonKind::Bounding,
    },
    ReasonSpec {
        reason: Reason::Cap,
        kind: ReasonKind::Selecting,
    },
];
const WALK_CAP: &[ReasonSpec] = &[
    ReasonSpec {
        reason: Reason::Walk,
        kind: ReasonKind::Bounding,
    },
    ReasonSpec {
        reason: Reason::Cap,
        kind: ReasonKind::Selecting,
    },
];
const WALK_BUDGET: &[ReasonSpec] = &[
    ReasonSpec {
        reason: Reason::Walk,
        kind: ReasonKind::Bounding,
    },
    ReasonSpec {
        reason: Reason::Budget,
        kind: ReasonKind::Selecting,
    },
];

pub const SURFACE_SPECS: &[SurfaceSpec] = &[
    SurfaceSpec {
        command: "callgraph",
        mode: "impact",
        list_id: "payload.sites",
        unit: Unit::Sites,
        narrow: &["depth", "includeTests"],
        reasons: CAP_DEPTH,
    },
    SurfaceSpec {
        command: "callgraph",
        mode: "callers",
        list_id: "payload.callers",
        unit: Unit::Items,
        narrow: &["depth", "includeTests"],
        reasons: CAP_DEPTH,
    },
    SurfaceSpec {
        command: "callgraph",
        mode: "call_tree",
        list_id: "payload.tree",
        unit: Unit::Items,
        narrow: &["depth", "includeTests"],
        reasons: CAP_DEPTH,
    },
    SurfaceSpec {
        command: "callgraph",
        mode: "trace_to",
        list_id: "payload.paths",
        unit: Unit::Paths,
        narrow: &["depth", "includeTests"],
        reasons: DEPTH_BUDGET_CAP,
    },
    SurfaceSpec {
        command: "callgraph",
        mode: "trace_data",
        list_id: "payload.hops",
        unit: Unit::Hops,
        narrow: &["depth"],
        reasons: DEPTH_BOUNDING,
    },
    SurfaceSpec {
        command: "search",
        mode: "",
        list_id: "payload.results",
        unit: Unit::Results,
        narrow: &["topK", "path", "includeTests"],
        reasons: BUDGET_CAP,
    },
    SurfaceSpec {
        command: "grep",
        mode: "",
        list_id: "payload.matches",
        unit: Unit::Rows,
        narrow: &["path", "include", "exclude"],
        reasons: WALK_CAP,
    },
    SurfaceSpec {
        command: "glob",
        mode: "",
        list_id: "payload.files",
        unit: Unit::Files,
        narrow: &["path"],
        reasons: WALK_CAP,
    },
    SurfaceSpec {
        command: "outline",
        mode: "files",
        list_id: "payload.files",
        unit: Unit::Files,
        narrow: &["path"],
        reasons: WALK_BUDGET,
    },
    SurfaceSpec {
        command: "inspect",
        mode: "",
        list_id: "payload.details",
        unit: Unit::Items,
        narrow: &["topK", "scope", "sections"],
        reasons: CAP_SELECTING,
    },
    SurfaceSpec {
        command: "bash",
        mode: "",
        list_id: "bash.output",
        unit: Unit::Lines,
        narrow: &[],
        reasons: CAP_SELECTING,
    },
];

#[derive(Clone, Debug)]
pub struct KindBoundCase {
    pub name: String,
    pub surface: SurfaceSpec,
    pub fired: Vec<Reason>,
    pub expected_reason: Reason,
    pub expected_causes: Vec<Reason>,
    pub expected_total: Total,
    pub envelope: Option<ListEnvelope>,
}

pub fn surface_name(surface: SurfaceSpec) -> String {
    if surface.mode.is_empty() {
        surface.command.to_string()
    } else {
        format!("{}.{}", surface.command, surface.mode)
    }
}

fn cause_name(causes: &[Reason]) -> String {
    let mut names = causes
        .iter()
        .map(|cause| cause.as_str())
        .collect::<Vec<_>>();
    names.sort();
    names.join("+")
}

fn sorted_causes(causes: &[Reason]) -> Vec<Reason> {
    let mut sorted = causes.to_vec();
    sorted.sort_by_key(|reason| std::cmp::Reverse(reason.precedence()));
    sorted
}

fn build_case_envelope(surface: SurfaceSpec, fired: &[Reason]) -> Option<ListEnvelope> {
    let has = |reason| fired.contains(&reason);
    match (surface.command, surface.mode) {
        ("callgraph", "impact" | "callers" | "call_tree") => {
            let total = if has(Reason::Cap) { 21 } else { 15 };
            let truncated = usize::from(has(Reason::Depth));
            build_callgraph_envelope(surface.unit, 15, total, truncated)
        }
        ("callgraph", "trace_to") => {
            let total = if has(Reason::Cap) { 60 } else { 15 };
            build_trace_to_envelope(15, total, has(Reason::Depth), has(Reason::Budget))
        }
        ("callgraph", "trace_data") => build_trace_data_envelope(5, has(Reason::Depth)),
        ("search", "") => build_search_envelope(10, has(Reason::Cap), has(Reason::Budget)),
        ("grep", "") => {
            if has(Reason::Walk) && has(Reason::Cap) {
                build_grep_envelope_from_parts(100, 100, 100, true, true, 0)
            } else if has(Reason::Walk) {
                build_grep_envelope_from_parts(12, 12, 12, false, true, 0)
            } else {
                build_grep_envelope_from_parts(100, 1204, 100, true, false, 0)
            }
        }
        ("glob", "") => {
            if has(Reason::Walk) && has(Reason::Cap) {
                build_glob_envelope(5, 105, true, true, 0)
            } else if has(Reason::Walk) {
                build_glob_envelope(12, 12, false, true, 0)
            } else {
                build_glob_envelope(5, 21, false, false, 0)
            }
        }
        ("outline", "files") => build_outline_files_envelope(
            412,
            if has(Reason::Budget) { 1568 } else { 0 },
            has(Reason::Budget),
            has(Reason::Walk),
            false,
            0,
        ),
        ("inspect", "") => build_inspect_envelope(2, 4),
        ("bash", "") => build_bash_output_envelope(61, 4000),
        other => panic!("no conformance builder for {other:?}"),
    }
}

fn expected_total(surface: SurfaceSpec, fired: &[Reason]) -> Total {
    let has = |reason| fired.contains(&reason);
    match (surface.command, surface.mode) {
        ("callgraph", "impact" | "callers" | "call_tree") => {
            if has(Reason::Depth) {
                Total::AtLeast(15)
            } else {
                Total::Exact(21)
            }
        }
        ("callgraph", "trace_to") => {
            let value = if has(Reason::Cap) { 60 } else { 15 };
            if has(Reason::Depth) || has(Reason::Budget) {
                Total::AtLeast(value)
            } else {
                Total::Exact(value)
            }
        }
        ("callgraph", "trace_data") => Total::AtLeast(5),
        ("search", "") => Total::AtLeast(if has(Reason::Cap) { 11 } else { 10 }),
        ("grep", "") => {
            if has(Reason::Walk) {
                Total::AtLeast(if has(Reason::Cap) { 100 } else { 12 })
            } else {
                Total::AtLeast(1204)
            }
        }
        ("glob", "") => {
            if has(Reason::Walk) {
                Total::AtLeast(if has(Reason::Cap) { 105 } else { 12 })
            } else {
                Total::Exact(21)
            }
        }
        ("outline", "files") => {
            if has(Reason::Walk) {
                Total::AtLeast(if has(Reason::Budget) { 1980 } else { 412 })
            } else {
                Total::Exact(1980)
            }
        }
        ("inspect", "") => Total::Exact(4),
        ("bash", "") => Total::Exact(4000),
        other => panic!("no expected total for {other:?}"),
    }
}

pub fn kind_bound_cases() -> Vec<KindBoundCase> {
    let mut cases = Vec::new();
    for &surface in SURFACE_SPECS {
        for mask in 1..(1usize << surface.reasons.len()) {
            let fired = surface
                .reasons
                .iter()
                .enumerate()
                .filter(|(index, _)| mask & (1 << index) != 0)
                .map(|(_, spec)| spec.reason)
                .collect::<Vec<_>>();
            let expected_causes = sorted_causes(&fired);
            let expected_reason = expected_causes[0];
            cases.push(KindBoundCase {
                name: format!("{}:{}", surface_name(surface), cause_name(&fired)),
                surface,
                expected_total: expected_total(surface, &fired),
                envelope: build_case_envelope(surface, &fired),
                fired,
                expected_reason,
                expected_causes,
            });
        }
    }
    cases
}

pub fn validate_surface_specs(observed: &[SurfaceSpec]) -> Vec<String> {
    let mut errors = Vec::new();
    if observed.len() != SURFACE_SPECS.len() {
        errors.push(format!(
            "surface contract count: expected {}, got {}",
            SURFACE_SPECS.len(),
            observed.len()
        ));
    }
    for expected in SURFACE_SPECS {
        let Some(actual) = observed.iter().find(|actual| {
            actual.command == expected.command
                && actual.mode == expected.mode
                && actual.list_id == expected.list_id
        }) else {
            errors.push(format!(
                "{}: surface contract is missing",
                surface_name(*expected)
            ));
            continue;
        };
        if actual.unit != expected.unit {
            errors.push(format!(
                "{}: surface unit expected {:?}, got {:?}",
                surface_name(*expected),
                expected.unit,
                actual.unit
            ));
        }
        let actual_reasons = actual
            .reasons
            .iter()
            .map(|entry| (entry.reason.as_str(), entry.kind))
            .collect::<BTreeMap<_, _>>();
        let expected_reasons = expected
            .reasons
            .iter()
            .map(|entry| (entry.reason.as_str(), entry.kind))
            .collect::<BTreeMap<_, _>>();
        if actual_reasons != expected_reasons {
            errors.push(format!(
                "{}: surface reason kinds expected {:?}, got {:?}",
                surface_name(*expected),
                expected_reasons,
                actual_reasons
            ));
        }
    }
    errors
}

pub fn validate_registry() -> Vec<String> {
    let mut errors = Vec::new();
    if LIST_SURFACES.len() != SURFACE_SPECS.len() {
        errors.push(format!(
            "surface inventory count: expected {}, got {}",
            SURFACE_SPECS.len(),
            LIST_SURFACES.len()
        ));
    }
    for expected in SURFACE_SPECS {
        let Some(actual) = LIST_SURFACES.iter().find(|actual| {
            actual.command == expected.command
                && actual.mode == expected.mode
                && actual.list_id == expected.list_id
        }) else {
            errors.push(format!(
                "{}: registered surface is missing",
                surface_name(*expected)
            ));
            continue;
        };
        if actual.unit != expected.unit {
            errors.push(format!(
                "{}: registered unit expected {:?}, got {:?}",
                surface_name(*expected),
                expected.unit,
                actual.unit
            ));
        }
        if actual.narrow != expected.narrow {
            errors.push(format!(
                "{}: narrow order expected {:?}, got {:?}",
                surface_name(*expected),
                expected.narrow,
                actual.narrow
            ));
        }
        let actual_reasons = actual
            .reasons
            .iter()
            .map(|entry| (entry.reason.as_str(), entry.kind))
            .collect::<BTreeMap<_, _>>();
        let expected_reasons = expected
            .reasons
            .iter()
            .map(|entry| (entry.reason.as_str(), entry.kind))
            .collect::<BTreeMap<_, _>>();
        if actual_reasons != expected_reasons {
            errors.push(format!(
                "{}: reason kinds expected {:?}, got {:?}",
                surface_name(*expected),
                expected_reasons,
                actual_reasons
            ));
        }
    }
    errors
}

pub fn validate_kind_bound_case(case: &KindBoundCase) -> Vec<String> {
    let mut errors = Vec::new();
    let Some(envelope) = &case.envelope else {
        errors.push(format!(
            "{}: capped path constructed no envelope",
            case.name
        ));
        return errors;
    };
    if envelope.reason != Some(case.expected_reason) {
        errors.push(format!(
            "{}: reason expected {:?}, got {:?}",
            case.name, case.expected_reason, envelope.reason
        ));
    }
    if envelope.causes != case.expected_causes {
        errors.push(format!(
            "{}: causes expected {:?}, got {:?}",
            case.name, case.expected_causes, envelope.causes
        ));
    }
    if envelope.total != case.expected_total {
        errors.push(format!(
            "{}: total expected {:?}, got {:?}",
            case.name, case.expected_total, envelope.total
        ));
    }
    if envelope.unit != case.surface.unit {
        errors.push(format!(
            "{}: unit expected {:?}, got {:?}",
            case.name, case.surface.unit, envelope.unit
        ));
    }
    let expected_narrow = case
        .surface
        .narrow
        .iter()
        .map(|value| value.to_string())
        .collect::<Vec<_>>();
    if envelope.narrow != expected_narrow {
        errors.push(format!(
            "{}: narrow expected {:?}, got {:?}",
            case.name, expected_narrow, envelope.narrow
        ));
    }
    let has_bounding = case.fired.iter().any(|reason| {
        case.surface
            .reasons
            .iter()
            .any(|entry| entry.reason == *reason && entry.kind == ReasonKind::Bounding)
    });
    if has_bounding && envelope.total.is_exact() {
        errors.push(format!(
            "{}: a Bounding cause cannot produce an Exact total",
            case.name
        ));
    }
    errors
}

#[derive(Clone, Debug)]
pub struct FixtureRecord {
    pub name: String,
    pub surface: SurfaceSpec,
    pub list_id: String,
    pub reply: Value,
    pub owner_path: Vec<String>,
    pub wire_key: String,
    pub rendered: String,
}

impl FixtureRecord {
    pub fn owner(&self) -> Option<&Map<String, Value>> {
        let mut cursor = &self.reply;
        for segment in &self.owner_path {
            cursor = cursor.get(segment)?;
        }
        cursor.as_object()
    }

    pub fn envelope_value(&self) -> Option<&Value> {
        self.owner()?.get(&self.wire_key)
    }

    pub fn envelope(&self) -> Option<ListEnvelope> {
        serde_json::from_value(self.envelope_value()?.clone()).ok()
    }
}

fn manifest_path(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(relative)
}

fn load_json(relative: &str) -> Value {
    let path = manifest_path(relative);
    let source = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
    serde_json::from_str(&source)
        .unwrap_or_else(|error| panic!("failed to parse {}: {error}", path.display()))
}

fn load_text(relative: &str) -> String {
    let path = manifest_path(relative);
    fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()))
}

fn response(data: &Value) -> Response {
    Response {
        id: data
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("conformance")
            .to_string(),
        success: true,
        data: data.clone(),
    }
}

fn render(command: &str, mode: &str, data: &Value) -> String {
    let mut context = FormatContext::default();
    if command == "callgraph" {
        context.callgraph_op = Some(mode.to_string());
    }
    if command == "outline" {
        context.outline_mode = OutlineMode::Files;
    }
    format_response_with_context(command, &response(data), &context)
}

fn registered_surface(command: &str, mode: &str) -> SurfaceSpec {
    *SURFACE_SPECS
        .iter()
        .find(|surface| surface.command == command && surface.mode == mode)
        .unwrap_or_else(|| panic!("missing test surface {command}.{mode}"))
}

fn push_root_fixture(
    fixtures: &mut Vec<FixtureRecord>,
    name: &str,
    command: &str,
    mode: &str,
    list_id: &str,
    reply: Value,
    wire_key: &str,
) {
    let rendered = render(command, mode, &reply);
    fixtures.push(FixtureRecord {
        name: name.to_string(),
        surface: registered_surface(command, mode),
        list_id: list_id.to_string(),
        reply,
        owner_path: Vec::new(),
        wire_key: wire_key.to_string(),
        rendered,
    });
}

pub fn capped_fixtures() -> Vec<FixtureRecord> {
    let mut fixtures = Vec::new();

    for name in [
        "more_available_only",
        "engine_capped_only",
        "both_flags_mixed",
    ] {
        let wrapped = load_json(&format!("tests/fixtures/search/{name}.json"));
        push_root_fixture(
            &mut fixtures,
            &format!("search/{name}"),
            "search",
            "",
            "payload.results",
            wrapped["data"].clone(),
            "results_list_envelope",
        );
    }

    for name in [
        "cap_100_of_1204",
        "walk_truncated_and_cap",
        "skipped_foreign_mounts_walk",
        "display_thinned_25_of_42",
    ] {
        push_root_fixture(
            &mut fixtures,
            &format!("grep/{name}"),
            "grep",
            "",
            "payload.matches",
            load_json(&format!("tests/fixtures/grep/{name}.json")),
            "matches_list_envelope",
        );
    }

    for name in [
        "executor_cap",
        "walk_and_cap",
        "skipped_foreign_mounts_r25",
        "walk_truncated",
        "display_files_per_dir_r24",
        "display_directories_r24",
    ] {
        push_root_fixture(
            &mut fixtures,
            &format!("glob/{name}"),
            "glob",
            "",
            "payload.files",
            load_json(&format!("tests/fixtures/glob/{name}.json")),
            "files_list_envelope",
        );
    }

    for name in [
        "budget_rollups",
        "nested_budget_rollup",
        "collection_truncated_alone",
        "collection_truncated_with_budget",
        "r1_boundary",
    ] {
        push_root_fixture(
            &mut fixtures,
            &format!("outline/{name}"),
            "outline",
            "files",
            "payload.files",
            load_json(&format!("tests/fixtures/outline/{name}.json")),
            "files_list_envelope",
        );
    }

    for name in [
        "depth_exhaustion_no_path",
        "budget_exhaustion",
        "budget_plus_max_depth",
        "cap_only_r26",
        "cap_plus_depth",
    ] {
        push_root_fixture(
            &mut fixtures,
            &format!("trace/{name}"),
            "callgraph",
            "trace_to",
            "payload.paths",
            load_json(&format!("tests/fixtures/trace/{name}.json")),
            "paths_list_envelope",
        );
    }
    push_root_fixture(
        &mut fixtures,
        "trace/trace_data_capped",
        "callgraph",
        "trace_data",
        "payload.hops",
        load_json("tests/fixtures/trace/trace_data_capped.json"),
        "hops_list_envelope",
    );

    for name in [
        "four_capped_lists",
        "empty_capped_list",
        "todos_capped",
        "duplicates_capped",
    ] {
        let wrapped = load_json(&format!("tests/fixtures/inspect/{name}.json"));
        let reply = wrapped["data"].clone();
        let rendered = render("inspect", "", &reply);
        let details = reply["details"]
            .as_object()
            .expect("inspect details object");
        for wire_key in details.keys().filter(|key| key.ends_with("_list_envelope")) {
            let list_key = wire_key.trim_end_matches("_list_envelope");
            fixtures.push(FixtureRecord {
                name: format!("inspect/{name}/{list_key}"),
                surface: registered_surface("inspect", ""),
                list_id: format!("payload.details.{list_key}"),
                reply: reply.clone(),
                owner_path: vec!["details".to_string()],
                wire_key: wire_key.clone(),
                rendered: rendered.clone(),
            });
        }
    }

    let bash_reply = load_json("tests/fixtures/bash/capped_4000_in_61_out/reply.json");
    push_root_fixture(
        &mut fixtures,
        "bash/capped_4000_in_61_out",
        "bash",
        "",
        "bash.output",
        bash_reply,
        "bash_output_list_envelope",
    );

    for case in kind_bound_cases()
        .into_iter()
        .filter(|case| case.surface.command == "callgraph")
    {
        let envelope = case.envelope.expect("callgraph case envelope");
        let wire_key = derive_wire_key(case.surface.list_id, false);
        let reply = json!({ wire_key.clone(): envelope });
        let rendered = aft::list_envelope::render_trailer(&envelope).unwrap_or_default();
        fixtures.push(FixtureRecord {
            name: format!("table/{}", case.name),
            surface: case.surface,
            list_id: case.surface.list_id.to_string(),
            reply,
            owner_path: Vec::new(),
            wire_key,
            rendered,
        });
    }

    let mut generated_search = json!({
        "text": "Found 10 result(s).",
        "results": [],
        "more_available": true,
        "engine_capped": false
    });
    attach_search_envelope(
        generated_search.as_object_mut().expect("search object"),
        10,
        true,
        false,
    );
    push_root_fixture(
        &mut fixtures,
        "product/search_attachment",
        "search",
        "",
        "payload.results",
        generated_search,
        "results_list_envelope",
    );

    let mut output = "first line".to_string();
    let envelope = append_envelope_trailer(&mut output, 5);
    let generated_bash = Value::Object(produce_bash_reply_data(output, envelope));
    push_root_fixture(
        &mut fixtures,
        "product/bash_attachment",
        "bash",
        "",
        "bash.output",
        generated_bash,
        "bash_output_list_envelope",
    );

    fixtures
}

pub fn canonical_trailer(envelope: &ListEnvelope) -> Option<String> {
    let reason = envelope.reason?;
    let total = match envelope.total {
        Total::Exact(value) if value < 1_000_000_000 => value.to_string(),
        Total::AtLeast(value) if value < 1_000_000_000 => format!("≥{value}"),
        Total::Exact(_) | Total::AtLeast(_) => "≥999999999".to_string(),
    };
    let mut trailer = format!(
        "shown {} of {} {} ({})",
        envelope.shown,
        total,
        envelope.unit.as_str(),
        reason.as_str()
    );
    if !envelope.narrow.is_empty() {
        trailer.push_str(" · narrow: ");
        trailer.push_str(&envelope.narrow.join(", "));
    }
    Some(trailer)
}

pub fn validate_fixture_schema(fixture: &FixtureRecord) -> Vec<String> {
    let mut errors = Vec::new();
    let Some(value) = fixture.envelope_value() else {
        errors.push(format!(
            "{}: missing capped envelope key {}",
            fixture.name, fixture.wire_key
        ));
        return errors;
    };
    let Some(object) = value.as_object() else {
        errors.push(format!("{}: envelope is not an object", fixture.name));
        return errors;
    };
    let keys = object.keys().map(String::as_str).collect::<BTreeSet<_>>();
    let expected_keys = ["shown", "total", "unit", "reason", "causes", "narrow"]
        .into_iter()
        .collect::<BTreeSet<_>>();
    if keys != expected_keys {
        errors.push(format!(
            "{}: envelope keys expected {:?}, got {:?}",
            fixture.name, expected_keys, keys
        ));
    }
    let total_keys = object
        .get("total")
        .and_then(Value::as_object)
        .map(|total| total.keys().map(String::as_str).collect::<BTreeSet<_>>());
    let expected_total_keys = ["kind", "value"].into_iter().collect::<BTreeSet<_>>();
    if total_keys.as_ref() != Some(&expected_total_keys) {
        errors.push(format!(
            "{}: total keys expected {:?}, got {:?}",
            fixture.name, expected_total_keys, total_keys
        ));
    }
    let Some(envelope) = fixture.envelope() else {
        errors.push(format!(
            "{}: envelope failed ListEnvelope decoding",
            fixture.name
        ));
        return errors;
    };
    if envelope.reason != envelope.causes.first().copied() {
        errors.push(format!("{}: reason must equal causes[0]", fixture.name));
    }
    if envelope.causes.is_empty() {
        errors.push(format!("{}: capped envelope has no causes", fixture.name));
    }
    if envelope
        .causes
        .windows(2)
        .any(|pair| pair[0].precedence() < pair[1].precedence())
    {
        errors.push(format!(
            "{}: causes are not in precedence order",
            fixture.name
        ));
    }
    let registered_reasons = fixture
        .surface
        .reasons
        .iter()
        .map(|entry| entry.reason)
        .collect::<Vec<_>>();
    for cause in &envelope.causes {
        if !registered_reasons.contains(cause) {
            errors.push(format!(
                "{}: envelope cause {:?} is not registered for the surface",
                fixture.name, cause
            ));
        }
    }
    if envelope.unit != fixture.surface.unit {
        errors.push(format!(
            "{}: envelope unit expected {:?}, got {:?}",
            fixture.name, fixture.surface.unit, envelope.unit
        ));
    }
    let expected_narrow = fixture
        .surface
        .narrow
        .iter()
        .map(|value| value.to_string())
        .collect::<Vec<_>>();
    if envelope.narrow != expected_narrow {
        errors.push(format!(
            "{}: envelope narrow expected {:?}, got {:?}",
            fixture.name, expected_narrow, envelope.narrow
        ));
    }
    if envelope.narrow.is_empty() != (fixture.surface.command == "bash") {
        errors.push(format!(
            "{}: narrow may be [] only for bash.output",
            fixture.name
        ));
    }
    errors
}

pub fn validate_trailer(fixture: &FixtureRecord) -> Vec<String> {
    let mut errors = Vec::new();
    let Some(envelope) = fixture.envelope() else {
        errors.push(format!("{}: no decodable envelope", fixture.name));
        return errors;
    };
    let expected = canonical_trailer(&envelope).expect("capped envelope reason");
    let actual = aft::list_envelope::render_trailer(&envelope);
    if actual.as_deref() != Some(expected.as_str()) {
        errors.push(format!(
            "{}: shared renderer expected {:?}, got {:?}",
            fixture.name, expected, actual
        ));
    }
    if !fixture.rendered.contains(&expected) {
        errors.push(format!(
            "{}: surface text omitted canonical trailer {:?}",
            fixture.name, expected
        ));
    }
    let words = expected.split_whitespace().collect::<Vec<_>>();
    if words.get(4).copied() != Some(envelope.unit.as_str()) {
        errors.push(format!(
            "{}: rendered unit does not equal envelope unit {:?}",
            fixture.name, envelope.unit
        ));
    }
    errors
}

fn walk_envelope_keys(value: &Value, path: &mut Vec<String>, errors: &mut Vec<String>) {
    match value {
        Value::Object(object) => {
            for (key, child) in object {
                if key == "list_envelope" {
                    errors.push(format!(
                        "{}: key literally named list_envelope is forbidden",
                        path.join(".")
                    ));
                } else if key.ends_with("_list_envelope") {
                    let valid = if key == "bash_output_list_envelope" {
                        LIST_SURFACES.iter().any(|surface| {
                            surface.list_id == "bash.output"
                                && derive_wire_key(surface.list_id, true) == *key
                        })
                    } else if path.last().map(String::as_str) == Some("details") {
                        LIST_SURFACES.iter().any(|surface| {
                            surface.command == "inspect"
                                && surface.list_id == "payload.details"
                                && key
                                    == &derive_wire_key(
                                        &format!(
                                            "{}.{}",
                                            surface.list_id,
                                            key.trim_end_matches("_list_envelope")
                                        ),
                                        false,
                                    )
                        })
                    } else {
                        LIST_SURFACES.iter().any(|surface| {
                            surface.command != "inspect"
                                && surface.command != "bash"
                                && derive_wire_key(surface.list_id, false) == *key
                        })
                    };
                    if !valid {
                        errors.push(format!(
                            "{}: envelope key {key} is not derivable from a registered list id",
                            path.join(".")
                        ));
                    }
                }
                path.push(key.clone());
                walk_envelope_keys(child, path, errors);
                path.pop();
            }
        }
        Value::Array(values) => {
            for (index, child) in values.iter().enumerate() {
                path.push(index.to_string());
                walk_envelope_keys(child, path, errors);
                path.pop();
            }
        }
        _ => {}
    }
}

pub fn validate_recursive_envelope_keys(value: &Value) -> Vec<String> {
    let mut errors = Vec::new();
    walk_envelope_keys(value, &mut Vec::new(), &mut errors);
    errors
}

pub fn validate_no_per_reason_drop_counts(value: &Value, rendered: &str) -> Vec<String> {
    fn visit(value: &Value, errors: &mut Vec<String>) {
        match value {
            Value::Object(object) => {
                for (key, child) in object {
                    let lower = key.to_ascii_lowercase();
                    for reason in ["cap", "budget", "depth", "walk"] {
                        let forbidden = [
                            format!("{reason}_dropped"),
                            format!("dropped_{reason}"),
                            format!("dropped_by_{reason}"),
                            format!("{reason}_drop_count"),
                        ];
                        if forbidden.iter().any(|needle| lower.contains(needle)) {
                            errors.push(format!("forbidden per-reason dropped-count key {key}"));
                        }
                    }
                    visit(child, errors);
                }
            }
            Value::Array(values) => {
                for child in values {
                    visit(child, errors);
                }
            }
            _ => {}
        }
    }

    let mut errors = Vec::new();
    visit(value, &mut errors);
    let lower = rendered.to_ascii_lowercase();
    for reason in ["cap", "budget", "depth", "walk"] {
        for phrase in [
            format!("dropped by {reason}"),
            format!("dropped for {reason}"),
            format!("{reason} dropped "),
        ] {
            if lower.contains(&phrase) {
                errors.push(format!(
                    "forbidden per-reason dropped-count text {phrase:?}"
                ));
            }
        }
    }
    errors
}

pub type PairKey = (String, String, String, String);

pub fn registered_pairs() -> BTreeSet<PairKey> {
    LIST_SURFACES
        .iter()
        .flat_map(|surface| {
            surface.reasons.iter().map(move |reason| {
                (
                    surface.command.to_string(),
                    surface.mode.to_string(),
                    surface.list_id.to_string(),
                    reason.reason.as_str().to_string(),
                )
            })
        })
        .collect()
}

pub fn fixture_pairs(cases: &[KindBoundCase]) -> BTreeSet<PairKey> {
    cases
        .iter()
        .flat_map(|case| {
            case.fired.iter().map(move |reason| {
                (
                    case.surface.command.to_string(),
                    case.surface.mode.to_string(),
                    case.surface.list_id.to_string(),
                    reason.as_str().to_string(),
                )
            })
        })
        .collect()
}

pub fn validate_pair_coverage(produced: &BTreeSet<PairKey>) -> Vec<String> {
    let registered = registered_pairs();
    let mut errors = Vec::new();
    for missing in registered.difference(produced) {
        errors.push(format!(
            "registered surface/reason has no fixture: {missing:?}"
        ));
    }
    for unknown in produced.difference(&registered) {
        errors.push(format!(
            "fixture produced unregistered surface/reason: {unknown:?}"
        ));
    }
    errors
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum CompletePathExemption {
    HonoredDepthCallgraphLegacyClause,
    DataHeavyOutlineRollupSentence,
    SyntheticThirdEntry,
}

pub const COMPLETE_PATH_EXEMPTIONS: &[CompletePathExemption] = &[
    CompletePathExemption::HonoredDepthCallgraphLegacyClause,
    CompletePathExemption::DataHeavyOutlineRollupSentence,
];

pub fn validate_exemption_list(entries: &[CompletePathExemption]) -> Vec<String> {
    if entries == COMPLETE_PATH_EXEMPTIONS {
        Vec::new()
    } else {
        vec![format!(
            "complete-path exemption set must be exactly {:?}, got {:?}",
            COMPLETE_PATH_EXEMPTIONS, entries
        )]
    }
}

#[derive(Clone, Debug)]
pub struct CompleteFixture {
    pub name: String,
    pub reply: Value,
    pub rendered: String,
    pub golden: String,
    pub exemption: Option<CompletePathExemption>,
}

fn make_impact_value(count: usize, depth_limited: bool) -> Value {
    let result = StoreImpactResult {
        symbol: "target".to_string(),
        file: "src/app.ts".to_string(),
        signature: None,
        parameters: Vec::new(),
        total_affected: count,
        hidden_test_callers: 0,
        affected_files: 1,
        callers: (1..=count)
            .map(|index| StoreImpactCaller {
                caller_symbol: format!("caller{index}"),
                caller_file: "src/app.ts".to_string(),
                line: index as u32,
                signature: None,
                is_entry_point: false,
                call_expression: None,
                parameters: Vec::new(),
                approximate: None,
                resolved_by: None,
            })
            .collect(),
        hub_summary: None,
        depth_limited,
        truncated: 0,
        sites_list_envelope: build_callgraph_envelope(Unit::Sites, count, count, 0),
    };
    serde_json::to_value(result).expect("impact fixture serialization")
}

fn make_callers_value(count: usize, depth_limited: bool) -> Value {
    let result = StoreCallersResult {
        symbol: "target".to_string(),
        file: "src/app.ts".to_string(),
        callers: vec![StoreCallerGroup {
            file: "src/app.ts".to_string(),
            callers: (1..=count)
                .map(|index| StoreCallerEntry {
                    symbol: format!("caller{index:02}"),
                    line: index as u32,
                    approximate: None,
                    resolved_by: None,
                })
                .collect(),
        }],
        total_callers: count,
        hidden_test_callers: 0,
        hub_summary: None,
        scanned_files: 1,
        depth_limited,
        truncated: 0,
        callers_list_envelope: build_callgraph_envelope(Unit::Items, count, count, 0),
    };
    serde_json::to_value(result).expect("callers fixture serialization")
}

fn make_tree_value(count: usize, depth_limited: bool) -> Value {
    let mut children = (1..=count)
        .map(|index| StoreCallTreeNode {
            name: format!("child{index}"),
            file: "src/app.ts".to_string(),
            line: (9 + index) as u32,
            signature: None,
            resolved: true,
            approximate: None,
            resolved_by: None,
            children: Vec::new(),
            depth_limited: false,
            truncated: 0,
            hidden_test_callers: 0,
            tree_list_envelope: None,
        })
        .collect::<Vec<_>>();
    let (shown, total) = cap_items(&mut children);
    let result = StoreCallTreeNode {
        name: "root".to_string(),
        file: "src/app.ts".to_string(),
        line: 1,
        signature: None,
        resolved: true,
        approximate: None,
        resolved_by: None,
        children,
        depth_limited,
        truncated: 0,
        hidden_test_callers: 0,
        tree_list_envelope: build_callgraph_envelope(Unit::Items, shown, total, 0),
    };
    serde_json::to_value(result).expect("call tree fixture serialization")
}

fn make_capped_impact_value(count: usize) -> Value {
    let callers = (1..=count)
        .map(|i| StoreImpactCaller {
            caller_symbol: format!("caller{i}"),
            caller_file: "src/app.ts".to_string(),
            line: i as u32,
            signature: None,
            is_entry_point: false,
            call_expression: None,
            parameters: Vec::new(),
            approximate: None,
            resolved_by: None,
        })
        .collect::<Vec<_>>();

    let summarize = hub_selector_activated(count);
    let visible_callers = if summarize {
        callers.into_iter().take(HUB_SUMMARY_LIMIT).collect()
    } else {
        callers
    };
    let shown = visible_callers.len();

    let hub_summary = if summarize {
        Some(StoreHubSummary {
            message: format!("Showing first 20 callers; {count} omitted high-fan-in callers."),
            total: count,
            hidden_tests: 0,
            shown,
            threshold: HUB_SUMMARY_THRESHOLD,
            limit: HUB_SUMMARY_LIMIT,
            counts_are_lower_bounds: false,
        })
    } else {
        None
    };

    let sites_list_envelope = build_callgraph_envelope(Unit::Sites, shown, count, 0);

    let result = StoreImpactResult {
        symbol: "target".to_string(),
        file: "src/app.ts".to_string(),
        signature: None,
        parameters: Vec::new(),
        total_affected: count,
        hidden_test_callers: 0,
        affected_files: 1,
        callers: visible_callers,
        hub_summary,
        depth_limited: false,
        truncated: 0,
        sites_list_envelope,
    };
    serde_json::to_value(result).expect("capped impact fixture serialization")
}

fn make_capped_callers_value(count: usize) -> Value {
    let entries = (1..=count)
        .map(|i| StoreCallerEntry {
            symbol: format!("caller{i:02}"),
            line: i as u32,
            approximate: None,
            resolved_by: None,
        })
        .collect::<Vec<_>>();

    let summarize = hub_selector_activated(count);
    let visible_entries = if summarize {
        entries.into_iter().take(HUB_SUMMARY_LIMIT).collect()
    } else {
        entries
    };
    let shown = visible_entries.len();

    let hub_summary = if summarize {
        Some(StoreHubSummary {
            message: format!("Showing first 20 callers; {count} omitted high-fan-in callers."),
            total: count,
            hidden_tests: 0,
            shown,
            threshold: HUB_SUMMARY_THRESHOLD,
            limit: HUB_SUMMARY_LIMIT,
            counts_are_lower_bounds: false,
        })
    } else {
        None
    };

    let callers_list_envelope = build_callgraph_envelope(Unit::Items, shown, count, 0);

    let result = StoreCallersResult {
        symbol: "target".to_string(),
        file: "src/app.ts".to_string(),
        callers: vec![StoreCallerGroup {
            file: "src/app.ts".to_string(),
            callers: visible_entries,
        }],
        total_callers: count,
        hidden_test_callers: 0,
        hub_summary,
        scanned_files: 1,
        depth_limited: false,
        truncated: 0,
        callers_list_envelope,
    };
    serde_json::to_value(result).expect("capped callers fixture serialization")
}

fn make_capped_tree_value(count: usize) -> Value {
    let mut children = (1..=count)
        .map(|i| StoreCallTreeNode {
            name: format!("child{i}"),
            file: "src/app.ts".to_string(),
            line: (9 + i) as u32,
            signature: None,
            resolved: true,
            approximate: None,
            resolved_by: None,
            children: Vec::new(),
            depth_limited: false,
            truncated: 0,
            hidden_test_callers: 0,
            tree_list_envelope: None,
        })
        .collect::<Vec<_>>();
    let (shown, total) = cap_items(&mut children);
    let tree_list_envelope = build_callgraph_envelope(Unit::Items, shown, total, 0);
    let result = StoreCallTreeNode {
        name: "root".to_string(),
        file: "src/app.ts".to_string(),
        line: 1,
        signature: None,
        resolved: true,
        approximate: None,
        resolved_by: None,
        children,
        depth_limited: false,
        truncated: 0,
        hidden_test_callers: 0,
        tree_list_envelope,
    };
    serde_json::to_value(result).expect("capped call tree fixture serialization")
}

#[derive(Clone, Debug)]
pub struct CappedParityFixture {
    pub name: &'static str,
    pub command: &'static str,
    pub mode: &'static str,
    pub list_id: &'static str,
    pub reply: Value,
}

pub fn capped_parity_fixtures() -> Vec<CappedParityFixture> {
    vec![
        CappedParityFixture {
            name: "callgraph/impact_21",
            command: "callgraph",
            mode: "impact",
            list_id: "payload.sites",
            reply: make_capped_impact_value(21),
        },
        CappedParityFixture {
            name: "callgraph/callers_21",
            command: "callgraph",
            mode: "callers",
            list_id: "payload.callers",
            reply: make_capped_callers_value(21),
        },
        CappedParityFixture {
            name: "callgraph/tree_21",
            command: "callgraph",
            mode: "call_tree",
            list_id: "payload.tree",
            reply: make_capped_tree_value(21),
        },
        CappedParityFixture {
            name: "trace/depth_exhaustion_no_path",
            command: "callgraph",
            mode: "trace_to",
            list_id: "payload.paths",
            reply: load_json("tests/fixtures/trace/depth_exhaustion_no_path.json"),
        },
        CappedParityFixture {
            name: "trace/trace_data_capped",
            command: "callgraph",
            mode: "trace_data",
            list_id: "payload.hops",
            reply: load_json("tests/fixtures/trace/trace_data_capped.json"),
        },
        CappedParityFixture {
            name: "search/more_available_only",
            command: "search",
            mode: "",
            list_id: "payload.results",
            reply: load_json("tests/fixtures/search/more_available_only.json")["data"].clone(),
        },
        CappedParityFixture {
            name: "grep/cap_100_of_1204",
            command: "grep",
            mode: "",
            list_id: "payload.matches",
            reply: load_json("tests/fixtures/grep/cap_100_of_1204.json"),
        },
        CappedParityFixture {
            name: "glob/executor_cap",
            command: "glob",
            mode: "",
            list_id: "payload.files",
            reply: load_json("tests/fixtures/glob/executor_cap.json"),
        },
        CappedParityFixture {
            name: "outline/r1_boundary",
            command: "outline",
            mode: "files",
            list_id: "payload.files",
            reply: load_json("tests/fixtures/outline/r1_boundary.json"),
        },
        CappedParityFixture {
            name: "inspect/todos_capped",
            command: "inspect",
            mode: "",
            list_id: "payload.details",
            reply: load_json("tests/fixtures/inspect/todos_capped.json")["data"].clone(),
        },
        CappedParityFixture {
            name: "bash/capped_4000_in_61_out",
            command: "bash",
            mode: "",
            list_id: "bash.output",
            reply: load_json("tests/fixtures/bash/capped_4000_in_61_out/reply.json"),
        },
    ]
}

pub fn validate_capped_parity_coverage(fixtures: &[CappedParityFixture]) -> Vec<String> {
    let mut errors = Vec::new();
    let covered: BTreeSet<(&str, &str, &str)> = fixtures
        .iter()
        .map(|f| (f.command, f.mode, f.list_id))
        .collect();

    for surface in SURFACE_SPECS {
        let key = (surface.command, surface.mode, surface.list_id);
        if !covered.contains(&key) {
            let label = if surface.mode.is_empty() {
                surface.command.to_string()
            } else {
                format!("{}.{}", surface.command, surface.mode)
            };
            errors.push(format!(
                "registered surface has no capped parity fixture: {label} ({})",
                surface.list_id
            ));
        }
    }
    errors
}

pub fn render_transports(fixture: &CappedParityFixture) -> (String, String) {
    let subc_text = render(fixture.command, fixture.mode, &fixture.reply);
    let base_text = if fixture.command == "bash" {
        fixture
            .reply
            .get("output")
            .or_else(|| fixture.reply.get("text"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    } else if fixture.command == "grep" {
        fixture
            .reply
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .replace(" (capped)", "")
    } else if fixture.command == "glob" {
        const GLOB_TRUNCATED_MESSAGE: &str =
            "(Results are truncated: showing first 100 results. Consider using a more specific path or pattern.)";
        fixture
            .reply
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .replace(&format!("\n\n{GLOB_TRUNCATED_MESSAGE}"), "")
            .replace(GLOB_TRUNCATED_MESSAGE, "")
    } else if fixture.command == "callgraph" {
        let wire_key = derive_wire_key(fixture.list_id, false);
        let env: Option<ListEnvelope> = fixture
            .reply
            .get(&wire_key)
            .and_then(|v| serde_json::from_value(v.clone()).ok());
        if let Some(trailer) = env
            .as_ref()
            .and_then(|e| aft::list_envelope::render_trailer(e))
        {
            subc_text
                .strip_suffix(&format!("\n\n{trailer}"))
                .or_else(|| subc_text.strip_suffix(&format!("\n{trailer}")))
                .unwrap_or(&subc_text)
                .to_string()
        } else {
            subc_text.clone()
        }
    } else {
        fixture
            .reply
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let is_text_surface = fixture.command == "bash";
    let list_id = if fixture.command == "inspect" {
        None
    } else {
        Some(fixture.list_id)
    };
    let mut ndjson_text = build_ndjson_text(&base_text, &fixture.reply, list_id, is_text_surface);
    if fixture.command == "callgraph"
        && (fixture.mode == "trace_to" || fixture.mode == "trace_data")
    {
        let wire_key = derive_wire_key(fixture.list_id, false);
        if let Some(val) = fixture.reply.get(&wire_key) {
            if let Ok(env) = serde_json::from_value::<ListEnvelope>(val.clone()) {
                if let Some(trailer) = aft::list_envelope::render_trailer(&env) {
                    ndjson_text =
                        ndjson_text.replace(&format!("\n\n{trailer}"), &format!("\n{trailer}"));
                }
            }
        }
    }
    (subc_text, ndjson_text)
}

pub fn render_complete_transports(fixture: &CompleteFixture) -> (String, String) {
    let subc_text = fixture.rendered.clone();
    let surface = SURFACE_SPECS.iter().find(|s| {
        if fixture.name.starts_with("callgraph/impact") {
            s.command == "callgraph" && s.mode == "impact"
        } else if fixture.name.starts_with("callgraph/caller") {
            s.command == "callgraph" && s.mode == "callers"
        } else if fixture.name.starts_with("callgraph/tree") {
            s.command == "callgraph" && s.mode == "call_tree"
        } else if fixture.name.starts_with("outline/") {
            s.command == "outline" && s.mode == "files"
        } else if fixture.name.starts_with("search/") {
            s.command == "search"
        } else if fixture.name.starts_with("grep/") {
            s.command == "grep"
        } else if fixture.name.starts_with("glob/") {
            s.command == "glob"
        } else if fixture.name.starts_with("inspect/") {
            s.command == "inspect"
        } else if fixture.name.starts_with("bash/") {
            s.command == "bash"
        } else {
            false
        }
    });
    let (list_id, is_text_surface) = if let Some(s) = surface {
        (
            if s.command == "inspect" {
                None
            } else {
                Some(s.list_id)
            },
            s.command == "bash",
        )
    } else {
        (None, false)
    };
    let base_text = if is_text_surface {
        fixture
            .reply
            .get("output")
            .or_else(|| fixture.reply.get("text"))
            .and_then(Value::as_str)
            .unwrap_or(&fixture.golden)
    } else if fixture.name.starts_with("callgraph/") {
        &subc_text
    } else {
        fixture
            .reply
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or(&fixture.golden)
    };
    let ndjson_text = build_ndjson_text(base_text, &fixture.reply, list_id, is_text_surface);
    (subc_text, ndjson_text)
}

fn push_complete(
    fixtures: &mut Vec<CompleteFixture>,
    name: &str,
    command: &str,
    mode: &str,
    reply: Value,
    golden: String,
    exemption: Option<CompletePathExemption>,
) {
    fixtures.push(CompleteFixture {
        name: name.to_string(),
        rendered: render(command, mode, &reply),
        reply,
        golden,
        exemption,
    });
}

pub fn complete_fixtures() -> Vec<CompleteFixture> {
    let mut fixtures = Vec::new();

    let search = load_json("tests/fixtures/search/complete_4_result.json")["data"].clone();
    let search_golden = search["text"].as_str().unwrap().to_string();
    push_complete(
        &mut fixtures,
        "search/complete_4_result",
        "search",
        "",
        search,
        search_golden,
        None,
    );

    for (command, name, path) in [
        (
            "grep",
            "grep/complete_untruncated",
            "tests/fixtures/grep/complete_untruncated.json",
        ),
        ("glob", "glob/complete", "tests/fixtures/glob/complete.json"),
    ] {
        let reply = load_json(path);
        let golden = reply["text"].as_str().unwrap().to_string();
        push_complete(&mut fixtures, name, command, "", reply, golden, None);
    }

    let inspect = load_json("tests/fixtures/inspect/complete_uncapped.json")["data"].clone();
    let inspect_golden = inspect["text"].as_str().unwrap().to_string();
    push_complete(
        &mut fixtures,
        "inspect/complete_uncapped",
        "inspect",
        "",
        inspect,
        inspect_golden,
        None,
    );

    let outline = load_json("tests/fixtures/outline/data_heavy_only.json");
    let outline_golden = outline["text"].as_str().unwrap().to_string();
    push_complete(
        &mut fixtures,
        "outline/data_heavy_only",
        "outline",
        "files",
        outline,
        outline_golden,
        Some(CompletePathExemption::DataHeavyOutlineRollupSentence),
    );

    let bash = load_json("tests/fixtures/bash/uncompressed/reply.json");
    let bash_golden = load_text("tests/fixtures/bash/uncompressed/input.txt");
    push_complete(
        &mut fixtures,
        "bash/uncompressed",
        "bash",
        "",
        bash,
        bash_golden,
        None,
    );

    for (mode, count, name, golden_name) in [
        (
            "impact",
            12,
            "callgraph/impact_12",
            "impact_12_expected.txt",
        ),
        (
            "impact",
            16,
            "callgraph/impact_16",
            "impact_16_expected.txt",
        ),
        (
            "impact",
            20,
            "callgraph/impact_20",
            "impact_20_expected.txt",
        ),
        (
            "callers",
            16,
            "callgraph/callers_16",
            "callers_16_expected.txt",
        ),
        (
            "callers",
            20,
            "callgraph/callers_20",
            "callers_20_expected.txt",
        ),
        (
            "call_tree",
            16,
            "callgraph/tree_16",
            "call_tree_16_expected.txt",
        ),
        (
            "call_tree",
            20,
            "callgraph/tree_20",
            "call_tree_20_expected.txt",
        ),
    ] {
        let reply = match mode {
            "impact" => make_impact_value(count, false),
            "callers" => make_callers_value(count, false),
            "call_tree" => make_tree_value(count, false),
            _ => unreachable!(),
        };
        let golden = load_text(&format!(
            "tests/fixtures/callgraph/truncation/{golden_name}"
        ));
        push_complete(&mut fixtures, name, "callgraph", mode, reply, golden, None);
    }

    for (mode, name, golden_name, reply) in [
        (
            "callers",
            "callgraph/callers_honored_depth",
            "callers_exemption_a_expected.txt",
            make_callers_value(12, true),
        ),
        (
            "call_tree",
            "callgraph/tree_honored_depth",
            "call_tree_exemption_a_expected.txt",
            make_tree_value(12, true),
        ),
    ] {
        let golden = load_text(&format!(
            "tests/fixtures/callgraph/truncation/{golden_name}"
        ));
        push_complete(
            &mut fixtures,
            name,
            "callgraph",
            mode,
            reply,
            golden,
            Some(CompletePathExemption::HonoredDepthCallgraphLegacyClause),
        );
    }

    fixtures
}

pub fn complete_builder_results() -> Vec<(&'static str, Option<ListEnvelope>)> {
    vec![
        (
            "callgraph.impact",
            build_callgraph_envelope(Unit::Sites, 20, 20, 0),
        ),
        (
            "callgraph.callers",
            build_callgraph_envelope(Unit::Items, 20, 20, 0),
        ),
        (
            "callgraph.call_tree",
            build_callgraph_envelope(Unit::Items, 20, 20, 0),
        ),
        (
            "callgraph.trace_to",
            build_trace_to_envelope(1, 1, false, false),
        ),
        ("callgraph.trace_data", build_trace_data_envelope(2, false)),
        ("search", build_search_envelope(4, false, false)),
        (
            "grep",
            build_grep_envelope_from_parts(4, 4, 4, false, false, 0),
        ),
        ("glob", build_glob_envelope(3, 3, false, false, 0)),
        (
            "outline.files",
            build_outline_files_envelope(3, 0, false, false, false, 0),
        ),
        ("inspect", build_inspect_envelope(3, 3)),
        ("bash", build_bash_output_envelope(5, 5)),
    ]
}

pub fn validate_hub_trigger(predicate: impl Fn(usize) -> bool) -> Vec<String> {
    let mut errors = Vec::new();
    for (count, expected) in [(15, false), (16, false), (20, false), (21, true)] {
        let actual = predicate(count);
        if actual != expected {
            errors.push(format!(
                "hub trigger at {count}: expected {expected}, got {actual}; trigger is not HUB_SUMMARY_LIMIT"
            ));
        }
    }
    if HUB_SUMMARY_LIMIT != 15 {
        errors.push(format!(
            "hub retained limit expected 15, got {HUB_SUMMARY_LIMIT}"
        ));
    }
    errors
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallSite {
    pub file: String,
    pub line: usize,
    pub code: String,
}

fn aft_src_dir() -> PathBuf {
    manifest_path("src")
}

fn collect_rs_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                files.extend(collect_rs_files(&path));
            } else if path.extension().and_then(|extension| extension.to_str()) == Some("rs") {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

fn registry_file_key(relative: &Path) -> String {
    relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

pub fn find_call_sites(function: &str) -> Vec<CallSite> {
    let source_dir = aft_src_dir();
    let mut sites = Vec::new();
    for path in collect_rs_files(&source_dir) {
        let source = fs::read_to_string(&path).unwrap_or_default();
        let file = registry_file_key(path.strip_prefix(&source_dir).unwrap_or(&path));
        let mut in_test_module = false;
        for (index, line) in source.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.starts_with("#[cfg(test)]") {
                in_test_module = true;
            }
            if in_test_module && trimmed.starts_with("mod tests") {
                break;
            }
            if trimmed.starts_with("pub fn ")
                || trimmed.starts_with("pub(crate) fn ")
                || trimmed.starts_with("fn ")
                || trimmed.starts_with("//")
                || trimmed.starts_with("/*")
                || trimmed.starts_with('*')
            {
                continue;
            }
            if trimmed.contains(&format!("{function}("))
                || trimmed.contains(&format!("{function} ("))
            {
                sites.push(CallSite {
                    file: file.clone(),
                    line: index + 1,
                    code: trimmed.to_string(),
                });
            }
        }
    }
    sites
}

pub fn validate_renderer_call_sites(
    render_sites: &[CallSite],
    measure_sites: &[CallSite],
) -> Vec<String> {
    let mut errors = Vec::new();
    let render_files = render_sites
        .iter()
        .map(|site| site.file.as_str())
        .collect::<Vec<_>>();
    let expected_render_files = ["list_envelope.rs", "ndjson_text.rs", "subc_format.rs"];
    if render_sites.len() != 3
        || expected_render_files
            .iter()
            .any(|file| !render_files.contains(file))
    {
        errors.push(format!(
            "render_trailer callers must be exactly {:?}, got {:?}",
            expected_render_files, render_sites
        ));
    }
    if measure_sites.len() != 1 || measure_sites[0].file != "list_surfaces/outline.rs" {
        errors.push(format!(
            "measure_trailer_len caller must be exactly list_surfaces/outline.rs, got {:?}",
            measure_sites
        ));
    }
    errors
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredCut {
    pub file: String,
    pub line: usize,
    pub enclosing_item: String,
    pub primitive: &'static str,
    pub snippet: String,
}

fn extract_item_name(line: &str) -> Option<String> {
    let trimmed = line.trim();
    let rest = [
        "pub(crate) fn ",
        "pub fn ",
        "fn ",
        "async fn ",
        "pub async fn ",
        "pub(crate) async fn ",
        "pub(crate) const ",
        "pub const ",
        "const ",
        "pub(crate) static ",
        "pub static ",
        "static ",
        "pub(crate) struct ",
        "pub struct ",
        "struct ",
        "pub(crate) enum ",
        "pub enum ",
        "enum ",
    ]
    .iter()
    .find_map(|prefix| trimmed.strip_prefix(prefix))?;
    let name = rest
        .chars()
        .take_while(|character| character.is_alphanumeric() || *character == '_')
        .collect::<String>();
    (!name.is_empty()).then_some(name)
}

fn enclosing_item(lines: &[&str], target: usize) -> String {
    (0..=target)
        .rev()
        .find_map(|index| extract_item_name(lines[index]))
        .unwrap_or_else(|| "<file_root>".to_string())
}

pub fn discover_list_cutting_sites() -> Vec<DiscoveredCut> {
    let source_dir = aft_src_dir();
    let mut paths = collect_rs_files(&source_dir.join("commands"));
    paths.push(source_dir.join("run_tool_call.rs"));
    paths.push(source_dir.join("subc_format.rs"));
    paths.extend(collect_rs_files(&source_dir.join("compress")));
    let primitives = [
        ".take(",
        "truncate(",
        "DEFAULT_MAX_RESULTS",
        "HUB_SUMMARY_LIMIT",
        "TRACE_TO_EXPANSION_BUDGET",
        "TRACE_TO_RETAINED_PATH_LIMIT",
        "walk_truncated",
        "skipped_foreign_mounts",
        "collection_truncated",
        "MAX_DISPLAY_",
    ];
    let mut discovered = Vec::new();
    for path in paths {
        if !path.exists() {
            continue;
        }
        let source = fs::read_to_string(&path).unwrap_or_default();
        let file = registry_file_key(path.strip_prefix(&source_dir).unwrap_or(&path));
        let lines = source.lines().collect::<Vec<_>>();
        for (index, line) in lines.iter().enumerate() {
            let trimmed = line.trim();
            if trimmed.starts_with("//") || trimmed.starts_with('*') {
                continue;
            }
            if trimmed.ends_with(".take();")
                || trimmed.contains(".take() ")
                || trimmed.contains(".take()?")
                || trimmed.contains(".take().")
                || trimmed.ends_with(".take()")
            {
                continue;
            }
            if let Some(primitive) = primitives
                .iter()
                .find(|primitive| trimmed.contains(**primitive))
            {
                discovered.push(DiscoveredCut {
                    file: file.clone(),
                    line: index + 1,
                    enclosing_item: enclosing_item(&lines, index),
                    primitive,
                    snippet: trimmed.to_string(),
                });
            }
        }
    }
    discovered
}

fn surface_matches_file(command: &str, file: &str) -> bool {
    match command {
        "grep" => file == "commands/grep.rs" || file == "subc_format.rs",
        "glob" => file == "commands/glob.rs" || file == "subc_format.rs",
        "callgraph" => file == "commands/callgraph_store_adapter.rs" || file == "subc_format.rs",
        "search" => file == "commands/semantic_search/mod.rs" || file == "subc_format.rs",
        "outline" => file == "commands/outline.rs" || file == "subc_format.rs",
        "inspect" => file == "commands/inspect.rs" || file == "subc_format.rs",
        "bash" => {
            file.starts_with("compress/") || file == "commands/bash.rs" || file == "subc_format.rs"
        }
        _ => false,
    }
}

pub fn resolve_cut(site: &DiscoveredCut) -> Result<&'static str, String> {
    for surface in LIST_SURFACES {
        if surface_matches_file(surface.command, &site.file)
            && surface.reasons.iter().any(|reason| {
                reason
                    .predicate_name
                    .split(',')
                    .map(str::trim)
                    .any(|predicate| predicate == site.enclosing_item)
            })
        {
            return Ok(surface.list_id);
        }
    }
    for exclusion in EXCLUSIONS {
        if (site.file == exclusion.file || site.file.ends_with(exclusion.file))
            && exclusion
                .enclosing_item
                .split(',')
                .map(str::trim)
                .any(|item| item == site.enclosing_item)
        {
            if exclusion.reason.trim().is_empty() {
                return Err(format!(
                    "exclusion for '{}:{}' has an empty reason",
                    exclusion.file, exclusion.enclosing_item
                ));
            }
            return Ok(exclusion.location_or_primitive);
        }
    }
    Err(format!(
        "unregistered list-cutting site at {}:{}: primitive '{}' in item '{}'",
        site.file, site.line, site.primitive, site.enclosing_item
    ))
}

pub fn validate_discovered_cuts(cuts: &[DiscoveredCut]) -> Vec<String> {
    cuts.iter()
        .filter_map(|cut| resolve_cut(cut).err())
        .collect()
}

pub fn validate_exclusion_reasons(exclusions: &[ExclusionEntry]) -> Vec<String> {
    exclusions
        .iter()
        .filter(|entry| entry.reason.trim().is_empty())
        .map(|entry| {
            format!(
                "exclusion for '{}:{}' has an empty reason",
                entry.file, entry.enclosing_item
            )
        })
        .collect()
}

pub fn canonical_hub_trigger(count: usize) -> bool {
    hub_selector_activated(count)
}
