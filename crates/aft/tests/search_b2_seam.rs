#[test]
fn readiness_disclosure_is_a_callback_but_never_a_retrieval_lane() {
    use aft::commands::semantic_search::extensions::{
        RawQuery, Readiness, RetainedReadinessSnapshots,
    };
    use aft::commands::semantic_search::plan_table::SearchLaneKind;
    use aft::search_b2::install_defaults;

    let raw_query = RawQuery::new("parse_header");
    let (shape, facts) = install_defaults().classify(&raw_query);
    let readiness = Readiness::observed(
        false,
        true,
        true,
        vec!["symbol:disabled".to_string()],
        RetainedReadinessSnapshots::default(),
        false,
    );
    let plan = install_defaults().plan(&shape, &facts, &readiness);
    assert_eq!(
        plan.selected_lanes,
        [
            SearchLaneKind::Exact,
            SearchLaneKind::Lexical,
            SearchLaneKind::Variants,
            SearchLaneKind::Semantic,
        ]
    );
    assert_eq!(
        plan.executed_callbacks,
        [
            SearchLaneKind::Exact,
            SearchLaneKind::Lexical,
            SearchLaneKind::Variants,
            SearchLaneKind::Semantic,
            SearchLaneKind::ReadinessDisclosure
        ]
    );
    assert!(!plan
        .selected_lanes
        .contains(&SearchLaneKind::ReadinessDisclosure));
}

#[test]
fn every_retrieval_callback_has_a_legal_selecting_request() {
    use aft::commands::semantic_search::extensions::{RawQuery, Readiness};
    use aft::commands::semantic_search::plan_table::SearchLaneKind;
    use aft::search_b2::install_defaults;

    let extensions = install_defaults();
    let ready = Readiness::new(true, true, true);
    let mut selected = std::collections::BTreeSet::new();
    for query in [
        "parse_header",
        "character cap",
        "2026-09-08 ERROR worker failed",
        "where do we cap the room name",
        "src/main.rs",
        "^export",
    ] {
        let raw_query = RawQuery::new(query);
        let (shape, facts) = extensions.classify(&raw_query);
        selected.extend(extensions.plan(&shape, &facts, &ready).selected_lanes);
    }
    let all_down = Readiness::new(false, false, false);
    let raw_query = RawQuery::new("parse_header");
    let (shape, facts) = extensions.classify(&raw_query);
    selected.extend(extensions.plan(&shape, &facts, &all_down).selected_lanes);

    assert_eq!(
        selected,
        SearchLaneKind::ALL
            .into_iter()
            .filter(|lane| *lane != SearchLaneKind::ReadinessDisclosure)
            .collect::<std::collections::BTreeSet<_>>()
    );
}

#[test]
fn natural_language_without_identifier_token_does_not_select_symbol() {
    use aft::commands::semantic_search::extensions::{RawQuery, Readiness};
    use aft::commands::semantic_search::plan_table::SearchLaneKind;
    use aft::search_b2::install_defaults;

    let extensions = install_defaults();
    let query = RawQuery::new("where does alpha beta gamma live in the service today");
    let (shape, facts) = extensions.classify(&query);
    let plan = extensions.plan(&shape, &facts, &Readiness::new(true, true, true));
    assert_eq!(shape.as_str(), "nl");
    assert!(!facts.has_identifier_token);
    assert!(!plan.selected_lanes.contains(&SearchLaneKind::Symbol));
}
