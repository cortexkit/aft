use aft::query_shape::{classify, QueryKind, ShapeWeights};

fn assert_shape(query: &str, kind: QueryKind, weights: ShapeWeights) {
    let first = classify(query);
    let second = classify(query);

    assert_eq!(first.kind, kind, "query: {query:?}");
    assert_weights(first.weights, weights, query);
    assert_eq!(
        first.kind, second.kind,
        "classification should be deterministic"
    );
    assert_weights(first.weights, second.weights, query);
}

fn assert_weights(actual: ShapeWeights, expected: ShapeWeights, query: &str) {
    assert!(
        (actual.semantic - expected.semantic).abs() <= f32::EPSILON,
        "semantic weight mismatch for {query:?}: {actual:?} vs {expected:?}"
    );
    assert!(
        (actual.lexical - expected.lexical).abs() <= f32::EPSILON,
        "lexical weight mismatch for {query:?}: {actual:?} vs {expected:?}"
    );
    assert_eq!(actual.should_use_lexical, expected.should_use_lexical);
}

fn weights(kind: QueryKind) -> ShapeWeights {
    classify(match kind {
        QueryKind::Identifier => "identifier",
        QueryKind::Mixed => "how does useState work",
        QueryKind::ErrorCode => "ERR_TIMEOUT",
        QueryKind::Path => "src/lib.rs",
        QueryKind::NaturalLanguage => "how does auth work",
    })
    .weights
}

#[test]
fn classifies_identifier_queries() {
    let expected = weights(QueryKind::Identifier);
    for query in [
        "useState",
        "aft_safety_history",
        "LSPManager",
        "foo.bar",
        "subagent_type",
        "getCurrentWorkingDirectory",
        "SearchIndex",
        "auth",
        "x",
        "API",
        "FOO_BAR",
        "two words",
        "  useEffect  ",
    ] {
        assert_shape(query, QueryKind::Identifier, expected);
    }
}

#[test]
fn classifies_path_queries_before_error_or_identifier_patterns() {
    let expected = weights(QueryKind::Path);
    for query in [
        "src/commands/grep.rs",
        "crates\\aft\\src\\lib.rs",
        "packages/opencode-plugin/src/tools/semantic.ts",
        "src/ERR_TIMEOUT.rs",
        "/tmp/E1234.log",
        "./foo/bar.json",
    ] {
        assert_shape(query, QueryKind::Path, expected);
    }
}

#[test]
fn classifies_error_code_queries_before_identifier_patterns() {
    let expected = weights(QueryKind::ErrorCode);
    for query in [
        "ERR_TIMEOUT",
        "E1234",
        "0xCAFE",
        "404",
        "HTTP 500",
        "ERR_CONNECTION_RESET",
        "0xdeadbeef panic",
        "E10000 failed",
    ] {
        assert_shape(query, QueryKind::ErrorCode, expected);
    }
}

#[test]
fn classifies_natural_language_queries() {
    let expected = weights(QueryKind::NaturalLanguage);
    for query in [
        "",
        "   ",
        "how does auth work",
        "what handles background task completion",
        "where is rate limiting handled",
        "why does indexing rebuild repeatedly",
        "when should semantic search run",
        "which module owns permissions",
        "who validates plugin tools",
        "does configure start indexing",
        "explain the authentication middleware flow",
    ] {
        assert_shape(query, QueryKind::NaturalLanguage, expected);
    }
}

#[test]
fn classifies_mixed_queries() {
    let expected = weights(QueryKind::Mixed);
    for query in [
        "how does useState work",
        "what calls aft_safety_history in tests",
        "where is LSPManager initialized",
        "why does foo.bar fail on startup",
        "does SearchIndex refresh stale files",
        "useState hook examples",
        "useState hook examples for cleanup",
    ] {
        assert_shape(query, QueryKind::Mixed, expected);
    }
}

#[test]
fn weights_are_stable_by_shape() {
    assert_weights(
        weights(QueryKind::Identifier),
        ShapeWeights {
            semantic: 0.2,
            lexical: 0.8,
            should_use_lexical: true,
        },
        "identifier",
    );
    assert_weights(
        weights(QueryKind::Path),
        ShapeWeights {
            semantic: 0.1,
            lexical: 0.9,
            should_use_lexical: true,
        },
        "path",
    );
    assert_weights(
        weights(QueryKind::Path),
        weights(QueryKind::ErrorCode),
        "error-code",
    );
    assert_weights(
        weights(QueryKind::NaturalLanguage),
        ShapeWeights {
            semantic: 0.6,
            lexical: 0.4,
            should_use_lexical: false,
        },
        "natural-language",
    );
    assert_weights(
        weights(QueryKind::Mixed),
        ShapeWeights {
            semantic: 0.4,
            lexical: 0.6,
            should_use_lexical: true,
        },
        "mixed",
    );
}
