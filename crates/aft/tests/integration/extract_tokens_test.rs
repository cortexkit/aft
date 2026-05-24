use aft::query_shape::{classify, extract_tokens, QueryKind};

fn tokens(query: &str) -> Vec<String> {
    let shape = classify(query);
    extract_tokens(query, &shape)
}

#[test]
fn worked_examples_match_pr3_token_contract() {
    assert_eq!(tokens("useState"), vec!["useState"]);
    assert_eq!(tokens("aft_safety_history"), vec!["aft_safety_history"]);
    assert_eq!(tokens("LSPManager"), vec!["LSPManager"]);
    assert_eq!(
        tokens("src/commands/grep.rs"),
        vec!["src", "commands", "grep", "grep.rs"]
    );
    assert_eq!(tokens("ERR_TIMEOUT"), vec!["ERR_TIMEOUT"]);

    let nl_shape = classify("how does auth work");
    assert_eq!(nl_shape.kind, QueryKind::NaturalLanguage);
    assert!(extract_tokens("how does auth work", &nl_shape).is_empty());

    assert_eq!(tokens("useState hook examples"), vec!["useState"]);
}
