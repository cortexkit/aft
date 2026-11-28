use aft::list_surfaces::ExclusionEntry;

// These out-of-line cfg(test) modules share directories with handlers. Only
// the listed fixture sites are exempt; their neighboring handlers still scan.
pub(super) const TEST_ONLY: &[ExclusionEntry] = &[
    ExclusionEntry {
        file: "commands/cache_location_tests/tests.rs",
        enclosing_item: "cache_location_concurrent_coordination_create_keeps_cache_available",
        location_or_primitive: "truncate(",
        reason:
            "The test configures how its coordination file is opened; it does not cut a response.",
    },
    ExclusionEntry {
        file: "commands/cache_location_tests/tests.rs",
        enclosing_item: "drop",
        location_or_primitive: ".take(",
        reason: "A fixture consumes a cleanup handle on drop.",
    },
    ExclusionEntry {
        file: "commands/grep_text_scope_tests/executor.rs",
        enclosing_item: "complete_fallback_walk_while_index_builds_says_every_file_was_searched",
        location_or_primitive: "walk_truncated",
        reason: "The test checks the fallback report; it does not choose the walk bound.",
    },
    ExclusionEntry {
        file: "commands/grep_text_scope_tests/executor.rs",
        enclosing_item: "bounded_fallback_walk_while_index_builds_says_it_is_incomplete",
        location_or_primitive: "walk_truncated",
        reason: "The test asserts the bounded walk reports incomplete results.",
    },
    ExclusionEntry {
        file: "commands/inspect_boundary_tests/cache.rs",
        enclosing_item: "assert_pre_campaign_aggregate_reopen_pages_all_findings",
        location_or_primitive: "truncate(",
        reason: "The expected fixture findings are sized to the comparison page.",
    },
    ExclusionEntry {
        file: "commands/outline_parser_tests/parser.rs",
        enclosing_item: "symbol_lines",
        location_or_primitive: ".take(",
        reason: "The test reads a prefix of the already-rendered outline for comparison.",
    },
    ExclusionEntry {
        file: "commands/outline_parser_tests/parser.rs",
        enclosing_item: "heading_columns_index_many_sections_with_mixed_endings",
        location_or_primitive: ".take(",
        reason: "The test selects fixture entries for its heading assertions.",
    },
    ExclusionEntry {
        file: "commands/outline_storage_tests.rs",
        enclosing_item: "a_matching_worktree_takes_its_siblings_tables_and_answers_the_same",
        location_or_primitive: ".take(",
        reason: "This collects a fixed test sample to compare stored and live navigation.",
    },
    ExclusionEntry {
        file: "commands/parent_query_tests.rs",
        enclosing_item: "parent_loading_child_bounded_walk_keeps_matches_and_reports_a_gap",
        location_or_primitive: "walk_truncated",
        reason: "The test asserts the child fallback reports the walk gap.",
    },
    ExclusionEntry {
        file: "commands/rewrite_tests/bash_background/secrets/tests.rs",
        enclosing_item: "drop",
        location_or_primitive: ".take(",
        reason: "A fixture consumes a cleanup handle on drop.",
    },
];
