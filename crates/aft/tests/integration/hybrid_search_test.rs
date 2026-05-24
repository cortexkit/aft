use std::path::PathBuf;

use aft::commands::semantic_search::fuse_hybrid_results;
use aft::query_shape::classify;
use aft::semantic_index::SemanticResult;
use aft::symbols::SymbolKind;

fn semantic(file: &str, name: &str, score: f32) -> SemanticResult {
    SemanticResult {
        file: PathBuf::from(file),
        name: name.to_string(),
        kind: SymbolKind::Function,
        start_line: 0,
        end_line: 2,
        exported: true,
        snippet: format!("fn {name}() {{}}"),
        score,
        source: "semantic",
    }
}

fn fingerprint(results: &[aft::commands::semantic_search::HybridResult]) -> Vec<String> {
    results
        .iter()
        .map(|result| {
            format!(
                "{}|{}|{}|{:.3}|{:?}|{:?}",
                result.file.display(),
                result.name,
                result.source,
                result.score,
                result.semantic_score,
                result.lexical_score
            )
        })
        .collect()
}

#[test]
fn identifier_file_in_both_lanes_gets_hybrid_boost() {
    let shape = classify("useState");
    let results = fuse_hybrid_results(
        vec![semantic("/project/src/hooks.ts", "useState", 0.4)],
        vec![(PathBuf::from("/project/src/hooks.ts"), 2.0)],
        &shape,
        10,
    );

    assert_eq!(results[0].source, "hybrid");
    assert_eq!(results[0].semantic_score, Some(0.4));
    assert_eq!(results[0].lexical_score, Some(2.0));
    assert!((results[0].score - 0.44).abs() < 0.0001);
}

#[test]
fn identifier_file_only_in_lexical_top_twenty_surfaces() {
    let shape = classify("useState");
    let results = fuse_hybrid_results(
        vec![semantic("/project/src/other.ts", "other", 0.3)],
        vec![(PathBuf::from("/project/src/hooks.ts"), 2.0)],
        &shape,
        10,
    );

    let lexical = results
        .iter()
        .find(|result| result.source == "lexical")
        .expect("lexical-only result");
    assert_eq!(lexical.file, PathBuf::from("/project/src/hooks.ts"));
    assert!(matches!(lexical.kind, SymbolKind::FileSummary));
    assert_eq!(lexical.name, "");
    assert_eq!(lexical.start_line, 0);
    assert_eq!(lexical.end_line, 0);
    assert!((lexical.score - 0.25).abs() < 0.0001);
}

#[test]
fn natural_language_query_with_no_lexical_lane_stays_semantic() {
    let shape = classify("how does auth work");
    let results = fuse_hybrid_results(
        vec![semantic("/project/src/auth.ts", "authorize", 0.7)],
        Vec::new(),
        &shape,
        10,
    );

    assert!(results.iter().all(|result| result.source == "semantic"));
}

#[test]
fn per_file_cap_keeps_top_two_results() {
    let shape = classify("useState");
    let results = fuse_hybrid_results(
        vec![
            semantic("/project/src/hooks.ts", "one", 0.9),
            semantic("/project/src/hooks.ts", "two", 0.8),
            semantic("/project/src/hooks.ts", "three", 0.7),
        ],
        vec![(PathBuf::from("/project/src/elsewhere.ts"), 0.5)],
        &shape,
        10,
    );
    let hooks_file = PathBuf::from("/project/src/hooks.ts");

    assert_eq!(
        results
            .iter()
            .filter(|result| result.file == hooks_file)
            .count(),
        2
    );
    assert!(results.iter().any(|result| result.name == "one"));
    assert!(results.iter().any(|result| result.name == "two"));
    assert!(!results.iter().any(|result| result.name == "three"));
}

#[test]
fn same_inputs_produce_stable_results() {
    let shape = classify("useState");
    let semantic_results = vec![
        semantic("/project/src/a.ts", "alpha", 0.5),
        semantic("/project/src/b.ts", "beta", 0.5),
    ];
    let lexical = vec![
        (PathBuf::from("/project/src/b.ts"), 1.0),
        (PathBuf::from("/project/src/c.ts"), 0.9),
    ];

    let first = fuse_hybrid_results(semantic_results.clone(), lexical.clone(), &shape, 10);
    let second = fuse_hybrid_results(semantic_results, lexical, &shape, 10);

    assert_eq!(fingerprint(&first), fingerprint(&second));
}
