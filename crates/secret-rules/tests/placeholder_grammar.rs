//! P1: the placeholder grammar.

use secret_rules::{contains_placeholder_prefix, is_label, mentions_label_loose, parse_at, scan};

#[test]
fn accepts_the_grammar_and_rejects_everything_else() {
    let accepted = [
        "x",
        "0",
        "a0",
        "repo_key",
        "repo-key",
        "repo.key",
        "a.b-c_d",
        &"a".repeat(63),
    ];
    for label in accepted {
        assert!(is_label(label), "should accept {label:?}");
    }

    let rejected = [
        "",
        "A",
        "Repo_Key",
        "-repo",
        ".repo",
        "_repo",
        "repo key",
        "repo/key",
        "repo:key",
        "repo$key",
        "repo`key",
        "repo!key",
        "repo*key",
        "repo\"key",
        "keyé",
        &"a".repeat(64),
        "{{ck-secret:x}}",
    ];
    for label in rejected {
        assert!(!is_label(label), "should reject {label:?}");
    }
}

#[test]
fn parses_a_placeholder_at_an_offset() {
    let text = "prefix {{ck-secret:repo_key}} suffix";
    let parsed = parse_at(text.as_bytes(), 7).expect("well-formed placeholder");
    assert_eq!(parsed.label, "repo_key");
    assert_eq!(parsed.text, "{{ck-secret:repo_key}}");

    assert!(parse_at(text.as_bytes(), 6).is_none());
    assert!(parse_at(text.as_bytes(), text.len()).is_none());
    assert!(parse_at(b"{{ck-secret:Repo}}", 0).is_none());
    assert!(parse_at(b"{{ck-secret:}}", 0).is_none());
    assert!(parse_at(b"{{ck-secret:repo}", 0).is_none());
    assert!(parse_at(b"{{ck-secret:repo}}", 1).is_none());
}

#[test]
fn scan_finds_every_well_formed_placeholder_in_order() {
    let text = "a {{ck-secret:one}} b {{ck-secret:Broken}} c {{ck-secret:{{lc:x}}}}";
    let found: Vec<&str> = scan(text)
        .iter()
        .map(|placeholder| placeholder.label)
        .collect();
    assert_eq!(
        found,
        vec!["one"],
        "a malformed mention is skipped, not guessed"
    );
}

#[test]
fn prefix_detection_covers_raw_and_encoded_forms() {
    assert!(contains_placeholder_prefix("curl -H 'x: {{ck-secret:a}}'"));
    assert!(contains_placeholder_prefix(
        "{{ck-secret:Broken}} still triggers"
    ));
    assert!(secret_rules::contains_placeholder_prefix_loose(
        "%7B%7Bck-secret%3Arepo%7D%7D"
    ));
    assert!(!contains_placeholder_prefix("just a command"));
}

#[test]
fn loose_mention_matching_ignores_case_and_expects_containment() {
    assert!(mentions_label_loose("{{ck-secret:repo_key}}", "repo_key"));
    assert!(mentions_label_loose("{{ck-secret:PROD_REPO}}", "repo"));
    assert!(mentions_label_loose("%7B%7Bck-secret%3ARepo%7D%7D", "repo"));
    assert!(!mentions_label_loose("{{ck-secret:other}}", "repo"));
    assert!(!mentions_label_loose("nothing here", "repo"));
}
