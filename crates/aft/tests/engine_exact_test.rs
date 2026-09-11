use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use aft::commands::semantic_search::evidence_descriptor::{EvidenceKind, EvidenceTier};
use aft::commands::semantic_search::generation_token::GenerationToken;
use aft::commands::semantic_search::handle_semantic_search;
use aft::config::Config;
use aft::context::AppContext;
use aft::parser::TreeSitterProvider;
use aft::protocol::RawRequest;
use aft::search_index::exact_lane::ExactLane;
use aft::search_index::memo::ExactMemoStore;
use aft::search_index::SearchIndex;

fn create_temp_corpus() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("create temp dir");
    let src = dir.path().join("src");
    fs::create_dir_all(&src).expect("create src dir");
    dir
}

/// Load-bearing non-vacuous specimen fixture:
/// Creates >= 60 files dense in query trigrams that fill the lexical top-50,
/// plus ONE file (rooms.rs) that contains the verbatim phrase but ranks below
/// lexical position 50 (lexical_rank_at_depth(50) does not contain it).
fn create_dense_trigram_corpus_with_specimen() -> (tempfile::TempDir, SearchIndex, PathBuf) {
    let dir = create_temp_corpus();
    let mut index = SearchIndex::new();

    // 65 decoy files dense in query trigrams (repeated 'character' and 'cap' separated,
    // but never the verbatim phrase "character cap")
    for i in 0..65 {
        let p = dir.path().join(format!("src/decoy_{i:03}.rs"));
        let mut text = String::new();
        text.push_str(&format!("// Decoy file {i}\n"));
        text.push_str("pub fn decoy_metric_calculation() {\n");
        // Inject dense individual trigrams so lexical score is high
        for _ in 0..30 {
            text.push_str("    let char_token = \"character\";\n");
            text.push_str("    let max_cap = \"cap\";\n");
            text.push_str("    let capacity = \"chars\";\n");
        }
        text.push_str("}\n");

        fs::write(&p, &text).unwrap();
        index.index_file(&p, text.as_bytes());
    }

    // Specimen file: rooms.rs
    // Has only ONE occurrence of "character cap" inside cap_chars,
    // so its lexical score is much lower than the 65 dense decoys.
    let rooms_path = dir.path().join("src/rooms.rs");
    let rooms_content = r#"
// Room definition
pub fn cap_chars(s: &str) -> String {
    // enforce character cap
    s.to_string()
}
"#;
    fs::write(&rooms_path, rooms_content).unwrap();
    index.index_file(&rooms_path, rooms_content.as_bytes());
    index.ready = true;

    (dir, index, rooms_path)
}

#[test]
fn test_alf_specimen_outside_lexical_top_50_found_rank_1_ready() {
    let (dir, index, _rooms_path) = create_dense_trigram_corpus_with_specimen();
    let snapshot = index.snapshot();

    let query = "character cap";
    let query_trigrams = SearchIndex::query_trigrams_from_tokens(&["character", "cap"]);

    // 1. Assert rooms.rs is strictly OUTSIDE the lexical top-50
    let lexical_top_50 = snapshot.lexical_rank_at_depth(&query_trigrams, None, 50);
    assert_eq!(
        lexical_top_50.files.len(),
        50,
        "lexical top-50 must be completely filled by the 65 dense decoys"
    );
    let rooms_in_top_50 = lexical_top_50
        .files
        .iter()
        .any(|(p, _)| p.file_name().unwrap() == "rooms.rs");
    assert!(
        !rooms_in_top_50,
        "rooms.rs must rank below position 50 in lexical rank (verified outside lexical top-50)"
    );

    // 2. Whole-corpus exact pass over trigram index in ready mode finds it at rank 1 with [exact]
    let lane = ExactLane::new();
    let result = lane.execute_ready_mode(&snapshot, dir.path(), query, false);

    assert!(
        !result.results.is_empty(),
        "whole-corpus exact pass must find rooms.rs"
    );
    let rank_1 = &result.results[0];
    assert_eq!(
        rank_1.path.file_name().unwrap().to_str().unwrap(),
        "rooms.rs"
    );
    assert_eq!(rank_1.evidence.tier, EvidenceTier::Exact);
    assert!(
        rank_1.symbol_range.is_some(),
        "rooms.rs matches cap_chars symbol"
    );
    // Reply carries no bounded disclosure in ready mode
    assert!(result.bound_disclosure.is_none());
}

#[test]
fn test_alf_specimen_beyond_grep_bounded_walk_file_limit_in_ready_mode() {
    let (dir, index, _rooms_path) = create_dense_trigram_corpus_with_specimen();

    // Set fallback file limit to 20 (well below the 66 corpus files).
    // In ready mode, the trigram index is ready, so ready mode does NOT use the walk.
    let lane = ExactLane {
        memo: Arc::new(ExactMemoStore::new()),
        fallback_file_limit: 20,
        fallback_result_limit: 10,
    };

    let query = "character cap";
    let token = GenerationToken::new(10);
    let outcome = lane
        .search(Some(&index), dir.path(), token, query, false, 0, 10, None)
        .expect("search");

    assert!(!outcome.results.is_empty());
    assert_eq!(
        outcome.results[0]
            .path
            .file_name()
            .unwrap()
            .to_str()
            .unwrap(),
        "rooms.rs"
    );
    assert_eq!(outcome.results[0].evidence.tier, EvidenceTier::Exact);
    // No bounded disclosure because ready mode does not use the walk
    assert!(outcome.bound_disclosure.is_none());
    assert!(!outcome.stability_void);
}

#[test]
fn test_alf_specimen_exceeds_cap_character_cap_rooms_over_status_line() {
    let dir = create_temp_corpus();
    let rooms_path = dir.path().join("src/rooms.rs");
    let status_path = dir.path().join("src/status_line.rs");

    // rooms.rs has verbatim E1 match for "exceeds the {cap}-character cap"
    fs::write(
        &rooms_path,
        r#"
pub fn check_room_name(name: &str) {
    if name.len() > 50 {
        panic!("exceeds the {cap}-character cap");
    }
}
"#,
    )
    .expect("write rooms.rs");

    // status_line.rs has E2 window match (content tokens across 3 lines)
    fs::write(
        &status_path,
        r#"
pub fn show_status(cap: usize) {
    let msg = "exceeds";
    let detail = "the limit";
    let note = "character cap";
}
"#,
    )
    .expect("write status_line.rs");

    let mut index = SearchIndex::new();
    index.index_file(&rooms_path, &fs::read(&rooms_path).unwrap());
    index.index_file(&status_path, &fs::read(&status_path).unwrap());
    index.ready = true;

    let lane = ExactLane::new();
    let snapshot = index.snapshot();
    let result = lane.execute_ready_mode(
        &snapshot,
        dir.path(),
        "exceeds the {cap}-character cap",
        false,
    );

    assert!(
        result.results.len() >= 2,
        "expected both rooms.rs and status_line.rs"
    );
    // rooms.rs (E1 verbatim) ranks before status_line.rs (E2 window)
    assert_eq!(
        result.results[0]
            .path
            .file_name()
            .unwrap()
            .to_str()
            .unwrap(),
        "rooms.rs"
    );
    assert_eq!(result.results[0].evidence.kind, EvidenceKind::E1);

    assert_eq!(
        result.results[1]
            .path
            .file_name()
            .unwrap()
            .to_str()
            .unwrap(),
        "status_line.rs"
    );
    assert_eq!(result.results[1].evidence.kind, EvidenceKind::E2);

    // Exact tier score fields are absent
    assert!(result.results[0].fusion_score.is_none());
    assert!(result.results[0].lane_score.is_none());
    assert!(result.results[1].fusion_score.is_none());
    assert!(result.results[1].lane_score.is_none());
}

#[test]
fn test_census_episodes_667_7617_15364_rank_1_exact() {
    let dir = create_temp_corpus();

    // Real episode data cited from .alfonso/data/aft-search-followup-census/episodes/

    // Episode 667 (.alfonso/data/aft-search-followup-census/episodes/0667)
    // Query: "ExecutableDescriptorObservationV2"
    // Target file: docs/chaos-aimock-bootstrap-v2-normative/schema-definitions.v1.json
    let schema_dir = dir.path().join("docs/chaos-aimock-bootstrap-v2-normative");
    fs::create_dir_all(&schema_dir).unwrap();
    let ep667_path = schema_dir.join("schema-definitions.v1.json");
    let ep667_content = r#"{
  "ExecutableDescriptorObservationV2": {
    "type": "object",
    "properties": {
      "openFlags": { "type": "integer" },
      "resolveFlags": { "type": "integer" },
      "before": { "type": "string" },
      "after": { "type": "string" },
      "stableIdentityEqual": { "type": "boolean" },
      "sizePolicy": { "type": "string" }
    }
  }
}
"#;
    fs::write(&ep667_path, ep667_content).unwrap();

    // Episode 7617 (.alfonso/data/aft-search-followup-census/episodes/7617)
    // Query: "was not found on PATH"
    // Target file: crates/aft/src/commands/configure.rs
    let cmd_dir = dir.path().join("crates/aft/src/commands");
    fs::create_dir_all(&cmd_dir).unwrap();
    let ep7617_path = cmd_dir.join("configure.rs");
    let ep7617_content = r#"
pub fn check_tool_path(tool: &str) {
    slog_warn!("configured tool {} was not found on PATH", tool);
}
"#;
    fs::write(&ep7617_path, ep7617_content).unwrap();

    // Episode 15364 (.alfonso/data/aft-search-followup-census/episodes/15364)
    // Query: "fn mint_id"
    // Target file: crates/prefrontal-core-store/src/lib.rs
    let store_dir = dir.path().join("crates/prefrontal-core-store/src");
    fs::create_dir_all(&store_dir).unwrap();
    let ep15364_path = store_dir.join("lib.rs");
    let ep15364_content = r#"
pub(crate) fn mint_id(conn: &Connection, prefix: &str) -> rusqlite::Result<String> {
    Ok(format!("{prefix}_{}", 1))
}
"#;
    fs::write(&ep15364_path, ep15364_content).unwrap();

    let mut index = SearchIndex::new();
    index.index_file(&ep667_path, ep667_content.as_bytes());
    index.index_file(&ep7617_path, ep7617_content.as_bytes());
    index.index_file(&ep15364_path, ep15364_content.as_bytes());
    index.ready = true;

    let lane = ExactLane::new();
    let snapshot = index.snapshot();

    // Episode 667 verification
    let r667 = lane.execute_ready_mode(
        &snapshot,
        dir.path(),
        "ExecutableDescriptorObservationV2",
        false,
    );
    assert!(!r667.results.is_empty(), "ep 667 must return result");
    assert_eq!(
        r667.results[0].path.file_name().unwrap().to_str().unwrap(),
        "schema-definitions.v1.json",
        "ep 667 target file must rank 1"
    );
    assert_eq!(r667.results[0].evidence.tier, EvidenceTier::Exact);

    // Episode 7617 verification
    let r7617 = lane.execute_ready_mode(&snapshot, dir.path(), "was not found on PATH", false);
    assert!(!r7617.results.is_empty(), "ep 7617 must return result");
    assert_eq!(
        r7617.results[0].path.file_name().unwrap().to_str().unwrap(),
        "configure.rs",
        "ep 7617 target file must rank 1"
    );
    assert_eq!(r7617.results[0].evidence.tier, EvidenceTier::Exact);

    // Episode 15364 verification
    let r15364 = lane.execute_ready_mode(&snapshot, dir.path(), "fn mint_id", false);
    assert!(!r15364.results.is_empty(), "ep 15364 must return result");
    assert_eq!(
        r15364.results[0]
            .path
            .file_name()
            .unwrap()
            .to_str()
            .unwrap(),
        "lib.rs",
        "ep 15364 target file must rank 1"
    );
    assert_eq!(r15364.results[0].evidence.tier, EvidenceTier::Exact);
}

#[test]
fn test_project_identity_isolation_different_roots() {
    let root_a = tempfile::tempdir().unwrap();
    let root_b = tempfile::tempdir().unwrap();

    let file_a = root_a.path().join("src/lib.rs");
    let file_b = root_b.path().join("src/lib.rs");
    fs::create_dir_all(file_a.parent().unwrap()).unwrap();
    fs::create_dir_all(file_b.parent().unwrap()).unwrap();

    let content = "pub fn shared_marker_function() { println!(\"identical contents\"); }\n";
    fs::write(&file_a, content).unwrap();
    fs::write(&file_b, content).unwrap();

    let memo_store = Arc::new(ExactMemoStore::new());
    let lane_a = ExactLane::with_memo(memo_store.clone());
    let lane_b = ExactLane::with_memo(memo_store.clone());

    let token = GenerationToken::new(1);

    // 1. Query root_a
    let outcome_a = lane_a
        .search(
            None,
            root_a.path(),
            token.clone(),
            "shared_marker_function",
            false,
            0,
            10,
            None,
        )
        .expect("search a");
    assert_eq!(memo_store.verifier_call_count(), 1);
    assert!(!outcome_a.results.is_empty());
    assert!(outcome_a.results[0].path.starts_with(root_a.path()));

    // 2. Query root_b with the exact same query and generation
    let outcome_b = lane_b
        .search(
            None,
            root_b.path(),
            token.clone(),
            "shared_marker_function",
            false,
            0,
            10,
            None,
        )
        .expect("search b");
    // Verifier MUST be called again because roots differ: counter becomes 2!
    assert_eq!(
        memo_store.verifier_call_count(),
        2,
        "root_b must not reuse root_a's memo entry"
    );
    assert!(!outcome_b.results.is_empty());
    assert!(outcome_b.results[0].path.starts_with(root_b.path()));
}

#[test]
fn test_project_identity_isolation_continuity_key() {
    let root_a = PathBuf::from("/projects/repo_a");
    let root_b = PathBuf::from("/projects/repo_b");

    let query = "common search phrase";
    let include_tests = false;
    let top_k = 10;

    // Continuity key: (project root, normalized query, includeTests, topK)
    let continuity_key_a = (root_a.clone(), query, include_tests, top_k);
    let continuity_key_b = (root_b.clone(), query, include_tests, top_k);

    assert_ne!(
        continuity_key_a, continuity_key_b,
        "distinct project roots must produce distinct continuity keys"
    );

    // Spurious disclosure test:
    // With project root in continuity key, a reload in A does not affect B
    let mut continuity_map = std::collections::HashMap::new();
    let gen_1 = GenerationToken::new(10);
    let gen_2 = GenerationToken::new(11);

    continuity_map.insert(continuity_key_a.clone(), gen_1.clone());
    continuity_map.insert(continuity_key_b.clone(), gen_1.clone());

    // Reload in root A only:
    let prev_a = continuity_map.get(&continuity_key_a);
    let spurious_in_b =
        prev_a != Some(&gen_2) && continuity_map.get(&continuity_key_b) != Some(&gen_1);
    assert!(
        !spurious_in_b,
        "no cross-root transition disclosure should fire in B"
    );
}

#[test]
fn exact_symbol_verification_clamps_utf8_span_boundaries() {
    let source = format!("fn unicode_body() {{\n{}\n}}\n", "═".repeat(300));
    let result = aft::commands::semantic_search::exact_lane::verify_exact_matches_in_text(
        std::path::Path::new("src/unicode.rs"),
        &source,
        "missing phrase",
        &[],
    );
    assert!(result.is_none());
}

/// CRLF line endings drift `lines()`-based offsets one byte left per line;
/// after enough lines the drifted symbol start lands inside a multibyte
/// character and slicing it panics the search actor (production, 2026-09-11,
/// on a 21 MB CRLF file with an em dash). Offsets must be true byte positions.
#[test]
fn exact_symbol_offsets_are_char_boundaries_on_crlf_multibyte_files() {
    use aft::commands::semantic_search::exact_lane::{
        scan_symbols_in_text, verify_exact_matches_in_text,
    };

    // Enough CRLF lines that the accumulated drift exceeds the width of the
    // multibyte characters in the final line, then a symbol line whose
    // computed start would fall inside one of them.
    let mut source = String::new();
    for index in 0..8 {
        source.push_str(&format!("// filler line {index}\r\n"));
    }
    source.push_str("// ——— em dashes before the symbol ———\r\n");
    source.push_str("fn after_dashes() {}\r\n");
    source.push_str("pub struct Trailing {}\r\n");

    let symbols = scan_symbols_in_text(&source);
    let names: Vec<&str> = symbols.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(names, ["after_dashes", "Trailing"]);
    for (name, range) in &symbols {
        assert!(
            source.is_char_boundary(range.start) && source.is_char_boundary(range.end),
            "{name}: range {range:?} is not boundary-aligned"
        );
        assert!(
            source[range.start..].starts_with("fn ")
                || source[range.start..].starts_with("pub struct "),
            "{name}: start {} does not point at the symbol line",
            range.start
        );
    }

    // The verifier must not panic on the same input, and must still find the
    // definition when the query names it.
    let result = verify_exact_matches_in_text(
        std::path::Path::new("src/crlf.rs"),
        &source,
        "after_dashes",
        &["after_dashes".to_string()],
    );
    assert!(
        result.is_some(),
        "definition on a CRLF file must still verify"
    );
}

#[test]
fn nl_quoted_span_exact_evidence_ranks_first() {
    let dir = create_temp_corpus();
    let target = dir.path().join("src/settle.rs");
    let phrase = "merged_ref is not integrated";
    fs::write(
        &target,
        format!(
            "pub const SETTLE_ERROR: &str = \"{phrase}\";\n// settle integration checks the campaign ref against caller HEAD\n"
        ),
    )
    .expect("write exact specimen");

    for ordinal in 0..44 {
        let decoy = dir.path().join(format!("src/decoy_{ordinal:02}.rs"));
        let dense = concat!(
            "settle refuses integration checked against which ref main campaign ",
            "integration_ref caller directory HEAD merged_ref integrated is not "
        )
        .repeat(40);
        fs::write(decoy, format!("// {dense}\n")).expect("write dense decoy");
    }

    let ctx = AppContext::new(
        Box::new(TreeSitterProvider::new()),
        Config {
            project_root: Some(dir.path().to_path_buf()),
            ..Config::default()
        },
    );
    *ctx.search_index()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(SearchIndex::build(dir.path()));

    let query = concat!(
        "settle refuses \"merged_ref is not integrated\": how is integration checked ",
        "(against which ref: main, the campaign integration_ref, or the caller directory HEAD)"
    );
    let request: RawRequest = serde_json::from_value(serde_json::json!({
        "id": "nl-quoted-span-exact-evidence",
        "command": "semantic_search",
        "query": query,
        "top_k": 5
    }))
    .expect("build engine request");
    let response = serde_json::to_value(handle_semantic_search(&request, &ctx))
        .expect("serialize engine response");

    assert_eq!(response["success"], true, "{response:?}");
    let results = response["results"].as_array().expect("results array");
    assert_eq!(results.len(), 5);
    // Rendered paths carry the host separator; the suffix check normalizes so
    // the assertion holds on Windows.
    assert!(results[0]["file"]
        .as_str()
        .is_some_and(|path| path.replace('\\', "/").ends_with("src/settle.rs")));
    assert_eq!(results[0]["exact"], true);
    assert!(response["text"]
        .as_str()
        .is_some_and(|text| text.contains("src/settle.rs [exact]")));
    assert!(results[1..].iter().all(|result| result["file"]
        .as_str()
        .is_some_and(|path| path.contains("/src/decoy_"))));
    assert_eq!(
        response["structuredContent"]["plan"]["shape"],
        "natural_language"
    );
    assert_eq!(response["structuredContent"]["plan"]["exact_input"], phrase);
}
