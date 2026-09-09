use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{json, Value};

mod comparator {
    pub use aft::commands::semantic_search::comparator::*;
}
mod evidence_descriptor {
    pub use aft::commands::semantic_search::evidence_descriptor::*;
}
mod plan_table {
    pub use aft::commands::semantic_search::plan_table::*;
}

#[path = "../src/commands/semantic_search/blocks.rs"]
mod blocks;
#[path = "../src/commands/semantic_search/paging.rs"]
mod paging;
#[path = "../src/commands/semantic_search/scoring.rs"]
mod scoring;

use blocks::{BlockBuilder, CanonicalLane, CanonicalListKey, LaneCandidate};
use evidence_descriptor::EvidenceDescriptor;
use paging::{parse_public_page_request, serve_public_page};
use plan_table::{PlanTable, SearchLaneKind, SearchShape};
use scoring::ScoringPolicy;

#[derive(Debug, Deserialize)]
struct Fixture {
    schema: u32,
    offset_cases: Vec<OffsetCase>,
    invalid_offsets: Vec<Value>,
    invalid_top_k: Value,
    continuity: ContinuityFixture,
    allowed_surface_runtime_edits: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct OffsetCase {
    offset: usize,
    top_k: usize,
    #[serde(default)]
    expected_first: Option<String>,
    #[serde(default)]
    expected_last: Option<String>,
    #[serde(default)]
    expected_empty: bool,
}

#[derive(Debug, Deserialize)]
struct ContinuityFixture {
    disclosure: String,
    before_token: String,
    after_token_same_index_component: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SurfaceContinuityKey {
    project_root: PathBuf,
    normalized_query: String,
    include_tests: bool,
    top_k: usize,
}

#[derive(Default)]
struct SurfaceContinuity {
    sessions: HashMap<String, HashMap<SurfaceContinuityKey, String>>,
}

impl SurfaceContinuity {
    fn render(
        &mut self,
        session: &str,
        key: SurfaceContinuityKey,
        generation: &str,
        page: &str,
        disclosure: &str,
    ) -> String {
        let session_map = self.sessions.entry(session.to_string()).or_default();
        let previous = session_map.insert(key, generation.to_string());
        if previous.as_deref().is_some_and(|token| token != generation) {
            format!("{disclosure}\n{page}")
        } else {
            page.to_string()
        }
    }
}

fn fixture() -> Fixture {
    serde_json::from_str(include_str!(
        "../../../benchmarks/aft-search/engine-fixtures/surface/cases.json"
    ))
    .expect("surface fixture must be valid JSON")
}

fn policy() -> ScoringPolicy {
    ScoringPolicy::from_plan_table(&PlanTable::running_table(), SearchShape::NaturalLanguage)
        .expect("running natural-language scoring policy")
}

fn backend() -> BlockBuilder {
    let candidates = (0..3200)
        .map(|position| {
            LaneCandidate::non_exact(
                format!("result-{position:04}.rs"),
                None,
                EvidenceDescriptor::for_non_exact(true, false),
                1.0 - position as f32 / 10_000.0,
                false,
            )
        })
        .collect();
    let lane =
        CanonicalLane::new(SearchLaneKind::Semantic, candidates).expect("canonical surface lane");
    BlockBuilder::new(
        CanonicalListKey {
            project_root: PathBuf::from("/virtual/surface-project"),
            snapshot_generation: "surface-generation".to_string(),
            normalized_query: "paged query".to_string(),
            include_tests: false,
        },
        policy(),
        vec![lane],
    )
    .expect("surface backend")
}

fn continuity_key(root: &str, query: &str) -> SurfaceContinuityKey {
    SurfaceContinuityKey {
        project_root: PathBuf::from(root),
        normalized_query: query
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_ascii_lowercase(),
        include_tests: false,
        top_k: 10,
    }
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root")
}

#[test]
fn agent_surface_offsets_match_direct_backend_pages_without_clamping() {
    let fixture = fixture();
    assert_eq!(fixture.schema, 1);
    let backend = backend();

    for case in fixture.offset_cases {
        let request = parse_public_page_request(&json!({
            "offset": case.offset,
            "topK": case.top_k,
        }))
        .expect("public surface request");
        assert_eq!(request.offset(), case.offset);
        assert_eq!(request.top_k(), case.top_k);
        let page = serve_public_page(&backend, request).expect("direct backend page");
        let paths = page
            .reply
            .page
            .iter()
            .map(|entry| entry.result.path.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        if case.expected_empty {
            assert!(
                paths.is_empty(),
                "large offsets must not be clamped to page zero"
            );
        } else {
            assert_eq!(paths.first(), case.expected_first.as_ref());
            assert_eq!(paths.last(), case.expected_last.as_ref());
        }
    }
}

#[test]
fn every_agent_surface_preserves_boundary_rejections() {
    let fixture = fixture();
    for offset in fixture.invalid_offsets {
        let error = parse_public_page_request(&json!({ "offset": offset, "topK": 10 }))
            .expect_err("invalid offset must be rejected");
        assert_eq!(error.code(), "invalid_request");
        assert_eq!(error.field(), "offset");
        assert!(error.to_string().contains("MAX_OFFSET"));
    }
    let error = parse_public_page_request(&json!({
        "offset": 0,
        "topK": fixture.invalid_top_k,
    }))
    .expect_err("existing topK bound must be retained");
    assert_eq!(error.code(), "invalid_request");
    assert_eq!(error.field(), "topK");
    assert!(error.to_string().contains("MAX_TOP_K"));
}

#[test]
fn surface_generation_continuity_is_session_query_and_project_isolated() {
    let fixture = fixture();
    let mut continuity = SurfaceContinuity::default();
    let key = continuity_key("/project/a", "same query");

    let first_a = continuity.render(
        "session-a",
        key.clone(),
        &fixture.continuity.before_token,
        "a-page-0",
        &fixture.continuity.disclosure,
    );
    let first_b = continuity.render(
        "session-b",
        key.clone(),
        &fixture.continuity.before_token,
        "b-page-0",
        &fixture.continuity.disclosure,
    );
    let changed_a = continuity.render(
        "session-a",
        key.clone(),
        &fixture.continuity.after_token_same_index_component,
        "a-page-1",
        &fixture.continuity.disclosure,
    );
    let stable_a = continuity.render(
        "session-a",
        key,
        &fixture.continuity.after_token_same_index_component,
        "a-page-2",
        &fixture.continuity.disclosure,
    );

    assert_eq!(first_a, "a-page-0");
    assert_eq!(first_b, "b-page-0");
    assert_eq!(
        changed_a,
        format!("{}\na-page-1", fixture.continuity.disclosure)
    );
    assert_eq!(stable_a, "a-page-2");

    let query_one = continuity.render(
        "interleaved",
        continuity_key("/project/a", "query one"),
        "query-one-generation",
        "one",
        &fixture.continuity.disclosure,
    );
    let query_two = continuity.render(
        "interleaved",
        continuity_key("/project/a", "query two"),
        "query-two-generation",
        "two",
        &fixture.continuity.disclosure,
    );
    let query_one_again = continuity.render(
        "interleaved",
        continuity_key("/project/a", "  QUERY   one "),
        "query-one-generation",
        "one-again",
        &fixture.continuity.disclosure,
    );
    assert_eq!(
        (query_one, query_two, query_one_again),
        ("one".into(), "two".into(), "one-again".into())
    );

    let root_one = continuity.render(
        "roots",
        continuity_key("/project/a", "root query"),
        "root-a-generation",
        "root-a",
        &fixture.continuity.disclosure,
    );
    let root_two = continuity.render(
        "roots",
        continuity_key("/project/b", "root query"),
        "root-b-generation",
        "root-b",
        &fixture.continuity.disclosure,
    );
    assert_eq!((root_one, root_two), ("root-a".into(), "root-b".into()));
}

#[test]
fn continuity_survives_restart_and_compares_the_whole_opaque_token() {
    let fixture = fixture();
    assert_ne!(
        fixture.continuity.before_token,
        fixture.continuity.after_token_same_index_component
    );
    let mut continuity = SurfaceContinuity::default();
    let key = continuity_key("/project/a", "restart query");
    assert_eq!(
        continuity.render(
            "restart-session",
            key.clone(),
            &fixture.continuity.before_token,
            "old-order-page",
            &fixture.continuity.disclosure,
        ),
        "old-order-page"
    );

    let first_after_restart = continuity.render(
        "restart-session",
        key.clone(),
        &fixture.continuity.after_token_same_index_component,
        "new-order-page",
        &fixture.continuity.disclosure,
    );
    let second_after_restart = continuity.render(
        "restart-session",
        key,
        &fixture.continuity.after_token_same_index_component,
        "next-new-order-page",
        &fixture.continuity.disclosure,
    );
    assert_eq!(
        first_after_restart,
        format!("{}\nnew-order-page", fixture.continuity.disclosure)
    );
    assert_eq!(second_after_restart, "next-new-order-page");
}

#[test]
fn surface_audit_locks_the_only_request_schema_change_and_runtime_hooks() {
    let fixture = fixture();
    assert_eq!(
        fixture.allowed_surface_runtime_edits,
        [
            "offset request forwarding",
            "per-session continuity map keyed by project root, normalized query, includeTests, and topK",
            "one-line generation-change disclosure",
        ]
    );

    let root = workspace_root();
    let pi = std::fs::read_to_string(root.join("packages/pi-plugin/src/tools/semantic.ts"))
        .expect("Pi semantic surface");
    let opencode =
        std::fs::read_to_string(root.join("packages/opencode-plugin/src/tools/semantic.ts"))
            .expect("OpenCode semantic surface");
    assert!(pi.contains("if (params.offset !== undefined) req.offset = params.offset;"));
    assert!(opencode.contains("if (offset !== undefined) rawArgs.offset = offset;"));
    for source in [&pi, &opencode] {
        assert!(
            source.contains("type SearchContinuityBySession = Map<string, Map<string, string>>")
        );
        assert!(source.contains("previous !== undefined && previous !== generation"));
        assert!(source.contains("GENERATION_CHANGED_DISCLOSURE"));
        assert!(!source.contains("continuation_token"));
        assert!(!source.contains("snapshot_generation.split"));
        assert!(!source.contains("parseInt(snapshot_generation"));
    }

    let artifact: Value = serde_json::from_str(
        &std::fs::read_to_string(root.join("crates/aft/src/subc_tool_schemas.json"))
            .expect("generated tool schema artifact"),
    )
    .expect("generated tool schema JSON");
    let search = &artifact["search"];
    let properties = search["properties"]
        .as_object()
        .expect("search schema properties");
    assert_eq!(
        properties.keys().cloned().collect::<HashSet<_>>(),
        ["query", "topK", "offset", "includeTests", "path"]
            .into_iter()
            .map(str::to_string)
            .collect()
    );
    assert_eq!(properties["offset"]["minimum"], 0);
    assert_eq!(properties["offset"]["maximum"], 100000);
    assert_eq!(properties["topK"]["minimum"], 1);
    assert_eq!(properties["topK"]["maximum"], 100);
    assert!(properties["path"]["description"]
        .as_str()
        .is_some_and(|description| description.contains("not a subdirectory filter")));
    assert_eq!(
        search["description"]
            .as_str()
            .expect("search description")
            .matches("Use `offset`")
            .count(),
        1
    );
}
