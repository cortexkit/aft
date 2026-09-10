use std::collections::BTreeMap;

use aft::commands::semantic_search::extensions::{SearchExtensions, Token, TokenVariant};
use aft::search_b2::variants::{
    contributing_variants, generate_variants, variants_footer, OrderedVariantPipeline,
    MAX_QUERY_VARIANTS,
};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct OrderedCase {
    input: String,
    expected: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct BackendCase {
    name: String,
    query: String,
    definition_present: bool,
    trigram_ready: bool,
    corpus: Vec<String>,
    expected_contributors: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ReachabilityFixture {
    episodes: Vec<u64>,
    oracle: BTreeMap<String, String>,
}

#[derive(Debug, Default)]
struct EmbeddingProbe {
    cache_accesses: usize,
    client_calls: usize,
}

fn generated_text(input: &str) -> Vec<String> {
    generate_variants(Token {
        index: 0,
        text: input,
    })
    .into_iter()
    .map(|variant| variant.text)
    .collect()
}

fn ordered_cases() -> Vec<OrderedCase> {
    serde_json::from_str(include_str!(
        "fixtures/search_b2/variants/ordered_cases.json"
    ))
    .unwrap()
}

fn backend_cases() -> Vec<BackendCase> {
    serde_json::from_str(include_str!(
        "fixtures/search_b2/variants/backend_cases.json"
    ))
    .unwrap()
}

fn run_variants_lane(
    case: &BackendCase,
    embedding: &mut EmbeddingProbe,
) -> (Vec<TokenVariant>, Vec<String>) {
    let _definition_result_does_not_gate_variants = case.definition_present;
    if !case.trigram_ready {
        return (Vec::new(), Vec::new());
    }

    let generated = OrderedVariantPipeline.variants(Token {
        index: 0,
        text: &case.query,
    });
    let admitted = generated
        .iter()
        .filter(|variant| {
            case.corpus
                .iter()
                .any(|document| document.contains(&variant.text))
        })
        .map(|variant| variant.text.clone())
        .collect::<Vec<_>>();

    assert_eq!(embedding.cache_accesses, 0);
    assert_eq!(embedding.client_calls, 0);
    (generated, admitted)
}

#[test]
fn six_declared_variant_arrays_are_literal_and_ordered() {
    let cases = ordered_cases();
    assert_eq!(cases.len(), 6);
    for case in cases {
        assert_eq!(generated_text(&case.input), case.expected, "{}", case.input);
    }
}

#[test]
fn direct_number_rule_asserts_key_to_keys() {
    assert_eq!(generated_text("key"), ["Key", "KEY", "keys"]);
    assert_eq!(generated_text("day"), ["Day", "DAY", "days"]);
}

#[test]
fn acronym_is_one_word_and_only_the_final_word_toggles() {
    let variants = generated_text("HTTPServer");
    assert_eq!(variants.last().map(String::as_str), Some("HTTPServers"));
    assert!(!variants.iter().any(|variant| variant.contains("h_t_t_p")));

    let variants = generated_text("itemKey");
    assert_eq!(
        variants,
        ["item_key", "item-key", "ItemKey", "ITEM_KEY", "itemKeys"]
    );
    assert!(!variants.iter().any(|variant| variant == "itemsKey"));
}

#[test]
fn hash_remainder_requires_the_declared_hex_prefix() {
    assert!(generated_text("#abc123_foo")
        .iter()
        .any(|value| value == "foo"));
    assert!(generated_text("abcdef-foo")
        .iter()
        .any(|value| value == "foo"));
    assert!(!generated_text("#abc12_foo")
        .iter()
        .any(|value| value == "foo"));
    assert!(!generated_text("#xyz123_foo")
        .iter()
        .any(|value| value == "foo"));
    assert!(!generated_text("#abc123foo")
        .iter()
        .any(|value| value == "foo"));
}

#[test]
fn separators_and_case_boundaries_are_the_only_word_boundaries() {
    assert_eq!(
        generated_text("foo.bar/baz-qux_quux"),
        [
            "foo_bar_baz_qux_quux",
            "foo-bar-baz-qux-quux",
            "fooBarBazQuxQuux",
            "FooBarBazQuxQuux",
            "FOO_BAR_BAZ_QUX_QUUX",
            "foo.bar/baz-qux_quuxes",
        ]
    );
}

#[test]
fn every_singular_exception_toggles_in_both_directions() {
    let pairs = [
        ("status", "statuses"),
        ("bus", "buses"),
        ("class", "classes"),
        ("process", "processes"),
        ("address", "addresses"),
        ("analysis", "analyses"),
        ("basis", "bases"),
        ("axis", "axes"),
        ("alias", "aliases"),
        ("canvas", "canvases"),
        ("focus", "focuses"),
        ("lens", "lenses"),
    ];
    for (singular, plural) in pairs {
        assert_eq!(
            generated_text(singular).last().map(String::as_str),
            Some(plural),
            "{singular}"
        );
        assert_eq!(
            generated_text(plural).last().map(String::as_str),
            Some(singular),
            "{plural}"
        );
    }
}

#[test]
fn first_emission_deduplication_and_query_wide_cap_are_shared() {
    assert_eq!(generated_text("items items"), ["Items", "ITEMS", "item"]);
    let variants = generate_variants(Token {
        index: 4,
        text: "character cap",
    });
    assert_eq!(variants.len(), MAX_QUERY_VARIANTS);
    assert_eq!(
        variants
            .iter()
            .map(|variant| variant.token_index)
            .collect::<Vec<_>>(),
        [4, 4, 4, 5, 5, 5]
    );

    let capped = generate_variants(Token {
        index: 0,
        text: "HTTPServer OtherThing",
    });
    assert_eq!(capped.len(), MAX_QUERY_VARIANTS);
    assert!(capped.iter().all(|variant| variant.token_index == 0));
}

#[test]
fn ready_backend_cases_ignore_definition_presence_and_never_embed() {
    let cases = backend_cases();
    let mut observed_ready_definition_states = Vec::new();

    for case in &cases {
        let mut embedding = EmbeddingProbe::default();
        let (generated, admitted) = run_variants_lane(case, &mut embedding);
        assert_eq!(admitted, case.expected_contributors, "{}", case.name);
        assert_eq!(embedding.cache_accesses, 0, "{}", case.name);
        assert_eq!(embedding.client_calls, 0, "{}", case.name);

        if case.trigram_ready {
            assert!(!generated.is_empty(), "{}", case.name);
            observed_ready_definition_states.push(case.definition_present);
        } else {
            assert!(generated.is_empty(), "{}", case.name);
            assert!(variants_footer(&generated, &admitted).is_none());
        }
    }

    observed_ready_definition_states.sort_unstable();
    observed_ready_definition_states.dedup();
    assert_eq!(observed_ready_definition_states, [false, true]);
}

#[test]
fn footer_contains_only_contributors_in_generation_order() {
    let case = backend_cases()
        .into_iter()
        .find(|case| case.name == "only admitted variants reach the footer")
        .unwrap();
    let mut embedding = EmbeddingProbe::default();
    let (generated, admitted) = run_variants_lane(&case, &mut embedding);

    assert_eq!(
        contributing_variants(&generated, &admitted),
        ["http-server", "HTTP_SERVER"]
    );
    assert_eq!(
        variants_footer(&generated, &admitted).as_deref(),
        Some("variants applied: http-server, HTTP_SERVER")
    );
}

#[test]
fn variant_episode_reachability_uses_the_declared_readiness_oracle() {
    let fixture: ReachabilityFixture = serde_json::from_str(include_str!(
        "fixtures/search_b2/variants/reachability.json"
    ))
    .unwrap();
    assert_eq!(fixture.episodes, [5985, 13820, 20065]);
    assert_eq!(
        fixture.oracle,
        BTreeMap::from([
            ("000".to_string(), "fallback_walk".to_string()),
            ("001".to_string(), "unreachable".to_string()),
            ("010".to_string(), "reachable_via:variants".to_string()),
            ("011".to_string(), "reachable_via:variants".to_string()),
            ("100".to_string(), "fallback_walk".to_string()),
            ("101".to_string(), "unreachable".to_string()),
            ("110".to_string(), "reachable_via:variants".to_string()),
            ("111".to_string(), "reachable_via:variants".to_string()),
        ])
    );
}
