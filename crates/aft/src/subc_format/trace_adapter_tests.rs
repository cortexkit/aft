mod trace_to_tests {
    use aft_engine::test_bridge::display_data::trace_budget::{
        impact_budget_fixture, impact_result_with_budget_for_test, IMPACT_BUDGET_NOTE_FOR_TEST,
    };
    use std::path::Path;

    #[test]
    fn impact_budget_preserves_complete_json_and_text_bytes() {
        let (store, target) = impact_budget_fixture();
        for depth in [1, 2, 5] {
            for include_tests in [false, true] {
                let legacy = impact_result_with_budget_for_test(
                    &store,
                    Path::new(&target.file),
                    &target.symbol,
                    depth,
                    include_tests,
                    usize::MAX,
                )
                .unwrap()
                .0;
                let (bounded, expansions) = impact_result_with_budget_for_test(
                    &store,
                    Path::new(&target.file),
                    &target.symbol,
                    depth,
                    include_tests,
                    100,
                )
                .unwrap();
                assert!(!bounded.counts_are_lower_bounds);
                assert!(expansions > 0 && expansions < 100);
                assert_eq!(
                    serde_json::to_vec(&bounded).unwrap(),
                    serde_json::to_vec(&legacy).unwrap()
                );
                assert_eq!(
                    crate::subc_format::format_callgraph(
                        "impact",
                        &serde_json::to_value(bounded).unwrap(),
                        false
                    ),
                    crate::subc_format::format_callgraph(
                        "impact",
                        &serde_json::to_value(legacy).unwrap(),
                        false
                    )
                );
            }
        }
    }

    #[test]
    fn impact_budget_stops_inside_a_frontier_and_marks_all_counts() {
        let (store, target) = impact_budget_fixture();
        let (bounded, expansions) = impact_result_with_budget_for_test(
            &store,
            Path::new(&target.file),
            &target.symbol,
            5,
            true,
            10,
        )
        .unwrap();
        assert_eq!(expansions, 10);
        assert_eq!(bounded.total_affected, 10);
        assert!(bounded.counts_are_lower_bounds);
        assert!(!bounded.incomplete.as_ref().unwrap().complete);
        assert_eq!(
            bounded.incomplete.as_ref().unwrap().incomplete_reason,
            IMPACT_BUDGET_NOTE_FOR_TEST
        );
        let envelope = bounded.sites_list_envelope.as_ref().unwrap();
        assert_eq!(envelope.total, crate::list_envelope::Total::AtLeast(10));
        assert_eq!(envelope.reason, Some(crate::list_envelope::Reason::Budget));
        assert!(bounded.root_counts.is_empty());
        let value = serde_json::to_value(bounded).unwrap();
        assert_eq!(value["complete"], false);
        let text = crate::subc_format::format_callgraph("impact", &value, false);
        assert!(text.contains("≥10 affected call sites"), "{text}");
        assert!(
            text.contains("shown 10 of ≥10 sites (budget) · narrow: depth, includeTests"),
            "{text}"
        );
    }
}
