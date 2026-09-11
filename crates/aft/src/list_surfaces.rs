use crate::list_envelope::{Reason, Unit};

pub mod bash;
pub mod call_tree;
pub mod callers;
pub mod glob;
pub mod grep;
pub mod impact;
pub mod inspect;
pub mod outline;
pub mod search;
pub mod trace_data;
pub mod trace_to;

/// Classification of a reason: whether the traversal stopped (`Bounding`)
/// or a selector chose which of an enumerated set to retain (`Selecting`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReasonKind {
    Bounding,
    Selecting,
}

/// A registered reason on a surface with its kind and the name of the function or constant where the condition is evaluated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReasonEntry {
    pub reason: Reason,
    pub kind: ReasonKind,
    pub predicate_name: &'static str,
}

/// Registry entry for a list-shaped surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SurfaceEntry {
    pub command: &'static str,
    pub mode: &'static str,
    pub list_id: &'static str,
    pub unit: Unit,
    pub narrow: &'static [&'static str],
    pub reasons: &'static [ReasonEntry],
}

/// Authoritative registry of every list-shaped surface in AFT.
pub static LIST_SURFACES: &[SurfaceEntry] = &[
    SurfaceEntry {
        command: "callgraph",
        mode: "impact",
        list_id: "payload.sites",
        unit: Unit::Sites,
        narrow: &["depth", "includeTests"],
        reasons: &[
            ReasonEntry {
                reason: Reason::Cap,
                kind: ReasonKind::Selecting,
                predicate_name: "HUB_SUMMARY_LIMIT, impact_result",
            },
            ReasonEntry {
                reason: Reason::Depth,
                kind: ReasonKind::Bounding,
                predicate_name: "depth_cut_inside_requested, truncated",
            },
        ],
    },
    SurfaceEntry {
        command: "callgraph",
        mode: "callers",
        list_id: "payload.callers",
        unit: Unit::Items,
        narrow: &["depth", "includeTests"],
        reasons: &[
            ReasonEntry {
                reason: Reason::Cap,
                kind: ReasonKind::Selecting,
                predicate_name: "HUB_SUMMARY_LIMIT, callers_result, test_hidden_summary, included_summary",
            },
            ReasonEntry {
                reason: Reason::Depth,
                kind: ReasonKind::Bounding,
                predicate_name: "depth_cut_inside_requested, truncated",
            },
        ],
    },
    SurfaceEntry {
        command: "callgraph",
        mode: "call_tree",
        list_id: "payload.tree",
        unit: Unit::Items,
        narrow: &["depth", "includeTests"],
        reasons: &[
            ReasonEntry {
                reason: Reason::Cap,
                kind: ReasonKind::Selecting,
                predicate_name: "HUB_SUMMARY_LIMIT",
            },
            ReasonEntry {
                reason: Reason::Depth,
                kind: ReasonKind::Bounding,
                predicate_name: "depth_cut_inside_requested, truncated",
            },
        ],
    },
    // Note: `trace_to_symbol` was originally grouped with `trace_to` under `payload.paths`,
    // but its reply is a single shortest path with no list semantics
    // (`path: Option<Vec<...>>`), rather than a `paths` list. Truncation envelopes apply
    // only to multi-path lists (`trace_to`).
    SurfaceEntry {
        command: "callgraph",
        mode: "trace_to",
        list_id: "payload.paths",
        unit: Unit::Paths,
        narrow: &["depth", "includeTests"],
        reasons: &[
            ReasonEntry {
                reason: Reason::Depth,
                kind: ReasonKind::Bounding,
                predicate_name: "trace_to_result, max_depth_reached",
            },
            ReasonEntry {
                reason: Reason::Budget,
                kind: ReasonKind::Bounding,
                predicate_name: "TRACE_TO_EXPANSION_BUDGET, trace_to_result_with_budget, lower_bound_trace_summary",
            },
            ReasonEntry {
                reason: Reason::Cap,
                kind: ReasonKind::Selecting,
                predicate_name: "TRACE_TO_RETAINED_PATH_LIMIT, retain_trace_path, trace_to_symbol_result",
            },
        ],
    },
    SurfaceEntry {
        command: "callgraph",
        mode: "trace_data",
        list_id: "payload.hops",
        unit: Unit::Hops,
        narrow: &["depth"],
        reasons: &[ReasonEntry {
            reason: Reason::Depth,
            kind: ReasonKind::Bounding,
            predicate_name: "depth_limited",
        }],
    },
    SurfaceEntry {
        command: "search",
        mode: "",
        list_id: "payload.results",
        unit: Unit::Results,
        narrow: &["offset", "topK", "path", "includeTests"],
        reasons: &[
            ReasonEntry {
                reason: Reason::Walk,
                kind: ReasonKind::Bounding,
                predicate_name: "SearchTrailer::shared_envelope_projection, StopState::S2Exhausted",
            },
            ReasonEntry {
                reason: Reason::Depth,
                kind: ReasonKind::Bounding,
                predicate_name: "SearchTrailer::shared_envelope_projection, StopState::S3DepthCap",
            },
            ReasonEntry {
                reason: Reason::Budget,
                kind: ReasonKind::Bounding,
                predicate_name: "engine_capped",
            },
            ReasonEntry {
                reason: Reason::Cap,
                kind: ReasonKind::Selecting,
                predicate_name: "SearchTrailer::shared_envelope_projection, StopState::S1MoreAtDepth, more_available, handle_external_semantic_or_hybrid_search, handle_external_grep_search, handle_semantic_or_hybrid_search, handle_grep_search, run_engine_ranking, view_semantic_search, blast_radius_annotation_for_result, enrich_snippets_from_source_reference, enrich_snippets_from_source_with_context, truncate_chars",
            },
        ],
    },
    SurfaceEntry {
        command: "grep",
        mode: "",
        list_id: "payload.matches",
        unit: Unit::Rows,
        narrow: &["path", "include", "exclude"],
        reasons: &[
            ReasonEntry {
                reason: Reason::Walk,
                kind: ReasonKind::Bounding,
                predicate_name: "handle_grep, grep_result, grep_result_bytes, walk_truncated, skipped_foreign_mounts",
            },
            ReasonEntry {
                reason: Reason::Cap,
                kind: ReasonKind::Selecting,
                predicate_name: "DEFAULT_MAX_RESULTS, MAX_DISPLAY_MATCHES_PER_FILE, MAX_DISPLAY_MATCHES, handle_grep, format_grep_text, rendered_grep_match_count",
            },
        ],
    },
    SurfaceEntry {
        command: "glob",
        mode: "",
        list_id: "payload.files",
        unit: Unit::Files,
        narrow: &["path"],
        reasons: &[
            ReasonEntry {
                reason: Reason::Walk,
                kind: ReasonKind::Bounding,
                predicate_name: "GlobDiscovery, handle_glob, fallback_glob, glob_root, walk_truncated, skipped_foreign_mounts",
            },
            ReasonEntry {
                reason: Reason::Cap,
                kind: ReasonKind::Selecting,
                predicate_name: "DEFAULT_MAX_RESULTS, MAX_DISPLAY_DIRECTORIES, MAX_DISPLAY_FILES_PER_DIRECTORY, handle_glob, format_glob_text, rendered_glob_file_count",
            },
        ],
    },
    SurfaceEntry {
        command: "outline",
        mode: "files",
        list_id: "payload.files",
        unit: Unit::Files,
        narrow: &["path"],
        reasons: &[
            ReasonEntry {
                reason: Reason::Budget,
                kind: ReasonKind::Selecting,
                predicate_name: "MAX_OUTPUT_BYTES, discover_outline_files, handle_outline_files_mode, budget_rollups_present",
            },
            ReasonEntry {
                reason: Reason::Walk,
                kind: ReasonKind::Bounding,
                predicate_name: "OutlineFileDiscovery, discover_outline_files_with_options, collect_outline_files_with_device_lookup, collect_outline_files_breadth_first_with_device_lookup, outline_walk_skips_and_reports_injected_foreign_mount, ITERATIONS, collection_truncated, walk_truncated, skipped_foreign_mounts",
            },
        ],
    },
    SurfaceEntry {
        command: "inspect",
        mode: "",
        list_id: "payload.details",
        unit: Unit::Items,
        narrow: &["topK", "scope", "sections"],
        reasons: &[ReasonEntry {
            reason: Reason::Cap,
            kind: ReasonKind::Selecting,
            predicate_name: "details_for, generated_details_for, test_only_details_for, topk_limiting",
        }],
    },
    SurfaceEntry {
        command: "bash",
        mode: "",
        list_id: "bash.output",
        unit: Unit::Lines,
        narrow: &[],
        reasons: &[ReasonEntry {
            reason: Reason::Cap,
            kind: ReasonKind::Selecting,
            predicate_name: "cap_lines, compress_json, finish, middle_truncate, append_hunk, cap_git_lines, compress_add, compress_blame, compress_diff, flush_status_entries, looks_like_golangci_json, finish_folded, first_error_lines, truncate_line, parse_tree, compress_tsc, frozen_compress_tsc, compressor_line_dropping",
        }],
    },
];

/// An entry in the discovery exclusions table with written justification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExclusionEntry {
    pub file: &'static str,
    pub enclosing_item: &'static str,
    pub location_or_primitive: &'static str,
    pub reason: &'static str,
}

/// Exclusions from the registry-free discovery scan with non-empty written reasons.
pub static EXCLUSIONS: &[ExclusionEntry] = &[
    // The lexical lane's depth tiers are engine-internal cuts over a candidate
    // pool (D_k = 200..3200); the agent never sees this list. The only cut an
    // agent sees is the search surface's topK, which carries the envelope.
    ExclusionEntry {
        file: "commands/semantic_search/lexical_lane.rs",
        enclosing_item: "from_scored_candidates",
        location_or_primitive: "depth-tier truncate",
        reason: "engine-internal lexical depth tier over the candidate pool; not an agent-visible list, the search surface attaches the envelope",
    },
    ExclusionEntry {
        file: "commands/semantic_search/lexical_lane.rs",
        enclosing_item: "enumerate_to_depth",
        location_or_primitive: "depth-tier take",
        reason: "engine-internal lexical depth tier over the candidate pool; not an agent-visible list, the search surface attaches the envelope",
    },
    ExclusionEntry {
        file: "commands/semantic_search/lexical_lane.rs",
        enclosing_item: "score_complete_selected_pool",
        location_or_primitive: "depth-tier take",
        reason: "engine-internal lexical depth tier over the candidate pool; not an agent-visible list, the search surface attaches the envelope",
    },
    // The block builder's page cut and the depth-tier observation are engine
    // cuts inside the search engine; the page the agent sees is the search
    // surface's, which attaches the envelope and the paging trailer.
    ExclusionEntry {
        file: "commands/semantic_search/blocks.rs",
        enclosing_item: "build",
        location_or_primitive: "page take",
        reason: "engine-internal page cut over the frozen block list; the search surface attaches the envelope and paging trailer",
    },
    ExclusionEntry {
        file: "commands/semantic_search/blocks.rs",
        enclosing_item: "observe_through_depth",
        location_or_primitive: "depth-tier take",
        reason: "engine-internal depth-tier observation over lane candidates; not an agent-visible list",
    },
    ExclusionEntry {
        file: "commands/semantic_search/paging.rs",
        enclosing_item: "build_l",
        location_or_primitive: "interval take",
        reason: "engine-internal interval cut when building L over frozen blocks; the served page's envelope and paging trailer are the search surface's",
    },
    ExclusionEntry {
        file: "commands/bash_status.rs",
        enclosing_item: "handle",
        location_or_primitive: "bash_status / bash live-tail",
        reason: "bash live-tail (bash_status polling) output is shown raw by design and carries no truncation envelope",
    },
    ExclusionEntry {
        file: "commands/status.rs",
        enclosing_item: "handle",
        location_or_primitive: "summary count arrays",
        reason: "summary counts and census count arrays are totals by construction and carry no truncation envelope",
    },
    ExclusionEntry {
        file: "commands/delete_file.rs",
        enclosing_item: "MAX_PATHS",
        location_or_primitive: "commands::delete_file::MAX_PATHS",
        reason: "error message preview of offending non-regular file paths is diagnostic formatting, not a returned list payload",
    },
    ExclusionEntry {
        file: "commands/read.rs",
        enclosing_item: "handle_directory",
        location_or_primitive: "commands::read::MAX_DIRECTORY_ENTRIES",
        reason: "raw directory entry read mode limits directory listing entries to MAX_DIRECTORY_ENTRIES",
    },
    ExclusionEntry {
        file: "commands/lsp_diagnostics.rs",
        enclosing_item: "build_response, compute_unchecked_files, handle_directory_mode",
        location_or_primitive: "commands::lsp_diagnostics::DIRECTORY_FILE_CAP",
        reason: "internal diagnostics scanner file resolution cap is an engine boundary, not a returned list",
    },
    ExclusionEntry {
        file: "commands/bash_orchestrate.rs",
        enclosing_item: "format_seconds",
        location_or_primitive: "commands::bash_orchestrate::seconds.truncate",
        reason: "string manipulation trimming timestamp representation, not a list truncation",
    },
    ExclusionEntry {
        file: "commands/zoom.rs",
        enclosing_item: "suggest_close_symbols",
        location_or_primitive: "commands::zoom::resolve_zoom_symbol candidate suggestion",
        reason: "did-you-mean candidate suggestion trimming for invalid symbol error messages",
    },
    ExclusionEntry {
        file: "commands/grep.rs",
        enclosing_item: "truncate_line_text",
        location_or_primitive: "commands::grep::truncate_line_text",
        reason: "line text preview truncation, not a list truncation",
    },
    ExclusionEntry {
        file: "commands/semantic_search/mod.rs",
        enclosing_item: "collect_degraded_grep_files, empty_degraded_grep_fallback_names_missing_semantic_coverage, execute_degraded_grep_fallback, handle_external_bounded_lexical_fallback, semantic_unavailable_grep_fallback_response",
        location_or_primitive: "commands::semantic_search degraded grep fallback status",
        reason: "internal fallback search status notes and degraded grep walk markers",
    },
    ExclusionEntry {
        file: "subc_format.rs",
        enclosing_item: "unresolved_summary_text",
        location_or_primitive: "subc_format::UNRESOLVED_SUMMARY_NAME_LIMIT",
        reason: "unresolved call site summary line preview names formatting",
    },
    ExclusionEntry {
        file: "subc_format.rs",
        enclosing_item: "format_outline_files_text",
        location_or_primitive: "subc_format::MAX_UNCHECKED_FILES_IN_FOOTER",
        reason: "legacy unchecked files list in outline text footer",
    },
    ExclusionEntry {
        file: "subc_format.rs",
        enclosing_item: "directory_outline_preserves_walk_truncation_footer, files_outline_uses_the_counting_walk_limit_in_partial_footer",
        location_or_primitive: "subc_format walk truncation footer tests",
        reason: "test assertions verifying legacy walk truncation footer",
    },
    ExclusionEntry {
        file: "commands/trace_to_symbol.rs",
        enclosing_item: "handle_trace_to_symbol",
        location_or_primitive: "commands::trace_to_symbol::handle_trace_to_symbol",
        reason: "trace_to_symbol reply is a single shortest path (path: Option<Vec<...>>) with no list semantics, so it carries no truncation envelope",
    },
];

/// Look up a surface by command, mode, and list id.
pub fn find_surface(command: &str, mode: &str, list_id: &str) -> Option<&'static SurfaceEntry> {
    LIST_SURFACES.iter().find(|s| {
        s.command == command
            && (s.mode == mode || s.mode.is_empty() || mode.is_empty())
            && s.list_id == list_id
    })
}
