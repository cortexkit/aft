use crate::list_envelope::{Reason, Unit};

pub mod bash;
pub mod call_tree;
pub mod callers;
pub mod glob;
pub mod grep;
pub mod impact;
pub mod inspect;
pub mod outline;
pub mod read;
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
                predicate_name: "SearchTrailer::shared_envelope_projection, StopState::S2Exhausted, execute_fallback_mode, ExternalFallbackBody, external_fallback_response",
            },
            ReasonEntry {
                reason: Reason::Depth,
                kind: ReasonKind::Bounding,
                predicate_name: "SearchTrailer::shared_envelope_projection, StopState::S3DepthCap",
            },
            ReasonEntry {
                reason: Reason::Budget,
                kind: ReasonKind::Bounding,
                // The regex route sets engine_capped in rank_collection when its
                // file-count or time bound left candidate files unexamined.
                predicate_name: "engine_capped, rank_collection",
            },
            // from_matches is the regex route's per-result line allowance: a
            // file lists its first matching lines and reports the rest in
            // more_in_file and a "+N more in this file" line.
            ReasonEntry {
                reason: Reason::Cap,
                kind: ReasonKind::Selecting,
                predicate_name: "SearchTrailer::shared_envelope_projection, StopState::S1MoreAtDepth, more_available, handle_external_semantic_or_hybrid_search, handle_external_grep_search, handle_semantic_or_hybrid_search, handle_grep_search, from_matches, run_engine_ranking, blast_radius_annotation_for_result, enrich_snippets_from_source_reference, enrich_snippets_from_source_with_context, truncate_chars, split_semantic_results, handle_split_search, rank_hits",
            },
        ],
    },
    SurfaceEntry {
        command: "grep",
        mode: "",
        list_id: "payload.matches",
        unit: Unit::Rows,
        narrow: &["offset", "path", "include", "exclude"],
        reasons: &[
            ReasonEntry {
                reason: Reason::Walk,
                kind: ReasonKind::Bounding,
                predicate_name: "handle_grep, grep_result, grep_result_bytes, walk_truncated, skipped_foreign_mounts",
            },
            ReasonEntry {
                reason: Reason::Cap,
                kind: ReasonKind::Selecting,
                predicate_name: "DEFAULT_MAX_RESULTS, GREP_MAX_OUTPUT_BYTES, MAX_DISPLAY_MATCHES_PER_FILE, handle_grep, format_grep_text, render_grep_page, rendered_grep_match_count",
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
                predicate_name: "MAX_OUTPUT_BYTES, discover_outline_files, handle_outline_files_mode, format_multi_file_tree, budget_rollups_present",
            },
            ReasonEntry {
                reason: Reason::Walk,
                kind: ReasonKind::Bounding,
                predicate_name: "OutlineFileDiscovery, discover_outline_files_with_options, collect_outline_files_with_device_lookup, collect_outline_files_breadth_first_with_device_lookup, outline_walk_skips_and_reports_injected_foreign_mount, ITERATIONS, collection_truncated, walk_truncated, skipped_foreign_mounts",
            },
        ],
    },
    SurfaceEntry {
        command: "read",
        mode: "directory",
        list_id: "payload.entries",
        unit: Unit::Items,
        narrow: &["path", "offset", "limit"],
        reasons: &[
            ReasonEntry {
                reason: Reason::Walk,
                kind: ReasonKind::Bounding,
                predicate_name: "handle_directory",
            },
            ReasonEntry {
                reason: Reason::Cap,
                kind: ReasonKind::Selecting,
                predicate_name: "handle_directory",
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
            predicate_name: "details_for, generated_details_for, test_only_details_for, uncovered_files_details_for, topk_limiting",
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
            predicate_name: "cap_lines, compress_json, finish, middle_truncate, append_hunk, cap_git_lines, compress_add, compress_blame, compress_diff, flush_status_entries, looks_like_golangci_json, finish_folded, first_error_lines, truncate_line, parse_tree, compress_tsc, frozen_compress_tsc, compressor_line_dropping, render_cut, cap_lines_head_tail, cap_text_head_tail, apply_plain_cap_streaming, test_verdict, cap_test_verdict",
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
    ExclusionEntry {
        file: "bash_background/remote.rs",
        enclosing_item: "append_executor_environment_disclosure",
        location_or_primitive: "executor environment name preview min(10)",
        reason: "supplemental execution note names at most ten filtered environment variables and explicitly counts the rest with +N more; the complete executor report remains in persisted task metadata, not a paginated tool-result list",
    },
    ExclusionEntry {
        file: "db/remote_exec.rs",
        enclosing_item: "sweep",
        location_or_primitive: "MAX_FROZEN_POLICIES / POLICY_SWEEP_BATCH SQL LIMIT",
        reason: "internal policy-retention maintenance scans bounded pages and persists its rotation cursor; it does not produce an agent-visible list or change an active scope's policy identity",
    },
    ExclusionEntry {
        file: "url_fetch.rs",
        enclosing_item: "hash_url",
        location_or_primitive: "URL cache hash prefix",
        reason: "cache filename derivation takes a fixed digest prefix, not a truncation of agent-visible text or records",
    },
    ExclusionEntry {
        file: "commands/url_output.rs",
        enclosing_item: "cap_text",
        location_or_primitive: "URL rendered text byte ceiling",
        reason: "remote document previews use an explicit truncated-at-N-of-M-bytes footer with URL and symbol narrowing advice; this is a byte cut, not a paginated list of records",
    },
    ExclusionEntry {
        file: "url_fetch.rs",
        enclosing_item: "read_response_body, fetch_url_to_cache",
        location_or_primitive: "URL download byte ceiling and UTF-8 prefix repair",
        reason: "the producer stops after a bounded prefix and one lookahead byte; cached metadata discloses the cut with the known size or an explicit lower bound, separately from the rendered output ceiling",
    },
    ExclusionEntry {
        file: "response_finalize.rs",
        enclosing_item: "enforce_reply_ceiling",
        location_or_primitive: "emergency rendered reply byte ceiling",
        reason: "a last-resort byte cut on every tool's rendered text, not a list surface; emits a cut-at-N-of-M-bytes notice and a WARN naming the tool whose own cap failed",
    },
    ExclusionEntry {
        file: "cold_build_limiter/progress.rs",
        enclosing_item: "snapshot",
        location_or_primitive: "take(LIST_CAP)",
        reason: "management health telemetry, not an agent list; running and queued are bounded at their iterators and disclose exact omitted counts",
    },
    ExclusionEntry {
        file: "github_read/fetch.rs",
        enclosing_item: "read_capped",
        location_or_primitive: "bounded gh subprocess reader",
        reason: "bounds stdout and stderr at the iterator; diff fetch ceilings are disclosed independently of line paging, and metadata ceilings refuse the fetch",
    },
    ExclusionEntry {
        file: "github_read/diff.rs",
        enclosing_item: "fetch_diff, page",
        location_or_primitive: "PR diff diagnostic paths and line window",
        reason: "diagnostic changed-path previews and sequential diff line pages integrate their list envelope trailers into content through the authorized NDJSON text builder",
    },
    ExclusionEntry {
        file: "grep_executor.rs",
        enclosing_item: "diagnose_scope_counts, bounded_fallback_walk_files_with_limits_target",
        location_or_primitive: "MAX_FALLBACK_WALK_FILES / FALLBACK_WALK_BUDGET",
        reason: "bounded filesystem fallback and empty-scope exclusion probe; ignored directories are pruned and counted once, and an exhausted probe reports unknown/incomplete rather than claiming an empty filesystem",
    },
    ExclusionEntry {
        file: "logging.rs",
        enclosing_item: "write_str",
        location_or_primitive: "PANIC_MESSAGE_BYTES, PANIC_BACKTRACE_BYTES",
        reason: "panic diagnostics bound formatted text, not a tool-result list; message and stack have separate byte budgets and an explicit truncation marker",
    },
    ExclusionEntry {
        file: "bash_db_hints/mod.rs",
        enclosing_item: "table_list, render, run_probe",
        location_or_primitive: "BLOCK_CAP, PROBE_OUTPUT_CAP",
        reason: "supplemental read-only schema trailer, not a paginated bash output list; omitted tables or schemas are explicitly counted, and an over-cap probe is discarded and counted as an error",
    },
    ExclusionEntry {
        file: "agent_child_env.rs",
        enclosing_item: "refresh_legacy_git_hooks",
        location_or_primitive: "legacy hook directory and file byte take",
        reason: "internal cache-maintenance enumeration and input-byte safety bounds, not an agent-visible list; the refresh logs examined and rewritten counts plus whether its scan was bounded",
    },
    // The views-on semantic gap note names its first few missing files in the
    // response text; every missing file is listed in the JSON `semantic_gap`
    // field, and the text says how many more there are.
    ExclusionEntry {
        file: "commands/semantic_search/mod.rs",
        enclosing_item: "disclose",
        location_or_primitive: "semantic gap note take",
        reason: "summary line naming the first few files a views-on semantic answer is missing, with its own '(+N more)' count; the full list is in the semantic_gap JSON field",
    },
    // The unanalyzed-macro note on `callers` is one summary line, not a list
    // the agent pages through: it states how many mentions exist (or that the
    // count is a lower bound) and says how many of them it spells out.
    ExclusionEntry {
        file: "commands/callgraph_store_adapter.rs",
        enclosing_item: "unanalyzed_macro_note",
        location_or_primitive: "macro note site truncate",
        reason: "summary line under the callers list that carries its own count and 'shown N of M' wording; the callers list itself keeps its envelope",
    },
    // The borrowed-graph coverage note on callgraph answers is one summary
    // line: it names the first few files the borrowed graph does not reflect
    // and says how many more there are.
    ExclusionEntry {
        file: "commands/callgraph_borrowed.rs",
        enclosing_item: "sample",
        location_or_primitive: "coverage note file sample take",
        reason: "summary line naming the first few files a borrowed callgraph does not reflect, with its own 'and N more' count; the counts are in the borrowed_coverage JSON field and the answer's own list keeps its envelope",
    },
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
        file: "commands/semantic_search/rerank/mod.rs",
        enclosing_item: "rerank_head",
        location_or_primitive: "page take",
        reason: "re-cuts the served page from the reranked canonical list with the same offset and topK as the block builder; the search surface computes the envelope and paging trailer from that page afterwards",
    },
    ExclusionEntry {
        file: "commands/semantic_search/rerank/mod.rs",
        enclosing_item: "rerank_positions",
        location_or_primitive: "rerank candidate take",
        reason: "selects which first-block entries a reranker may reorder (at most the configured rerank top_n); nothing is dropped from the list, so the served page and its paging trailer are unchanged in length",
    },
    ExclusionEntry {
        file: "commands/semantic_search/rerank/pool_export.rs",
        enclosing_item: "entries",
        location_or_primitive: "benchmark pool export take",
        reason: "benchmark-only export, written only when AFT_RERANK_POOL_EXPORT is set, of a fixed number of first-block candidates to a file; it is never part of a tool response",
    },
    ExclusionEntry {
        file: "commands/semantic_search/rerank/tests.rs",
        enclosing_item: "onnx_cost_profile_when_available",
        location_or_primitive: "test candidate text truncate",
        reason: "test-only cost profile that trims each candidate's text to a byte budget before timing the local reranker; no tool response is produced",
    },
    ExclusionEntry {
        file: "commands/semantic_search/exact_lane.rs",
        enclosing_item: "copied_worktree_exact_fallback_reproduction",
        location_or_primitive: "test walk-list truncate",
        reason: "ignored manual reproduction test that replays the former 1,000-file fallback walk cap to compare it with the current walk; no tool response is produced",
    },
    // Checkpoint and restore results name their first files and then say how
    // many more there are; the checkpoint itself always covers every file.
    ExclusionEntry {
        file: "subc_format.rs",
        enclosing_item: "checkpoint_path_lines",
        location_or_primitive: "checkpoint path lines take",
        reason: "summary of a checkpoint or restore result naming its first few files with its own '… and N more' count; the operation covers every file and no paging applies",
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
        file: "commands/outline.rs",
        enclosing_item: "render_top_level_entries",
        location_or_primitive: "type member preview take",
        reason: "a type API preview with an exact '(N more)' count; single-file outlines list every product member, and the outer file budget carries the list trailer",
    },
    ExclusionEntry {
        file: "commands/outline.rs",
        enclosing_item: "inspect_outline_file_content",
        location_or_primitive: "bounded line-count byte reader",
        reason: "caps bytes inspected for a file's line-count statistic; the file remains in the outline with an unknown line count, so no agent-visible list items are removed",
    },
    ExclusionEntry {
        file: "commands/read.rs",
        enclosing_item: "handle_streaming_range_read",
        location_or_primitive: "bounded streamed line window",
        reason: "limits bytes retained while reading a selected text line; truncated content and scan gaps are disclosed by the read response, not a cut to a list of files or result records",
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
        enclosing_item: "structure_outline_trailer_stays_last_after_skips",
        location_or_primitive: "walk_truncated",
        reason: "test-only response fixture proving that an integrated structure-map trailer remains last after skipped-file diagnostics; production outline walk and budget cuts are registered on the outline surface",
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
    ExclusionEntry {
        file: "response_finalize.rs",
        enclosing_item: "drop",
        location_or_primitive: "response_finalize::DeferredWakeGuard drop self.0.take()",
        reason: "Option::take restoring the previous deferred-completion wake when a guard drops; no list is cut",
    },
    ExclusionEntry {
        file: "commands/configure.rs",
        enclosing_item: "schedule_artifact_loads_admitted",
        location_or_primitive: "commands::configure::schedule_artifact_loads_admitted warm_permit.take()",
        reason: "Option::take releasing a warm-reload permit before a cold-build acquire; no list is cut",
    },
    ExclusionEntry {
        file: "commands/semantic_search/lexical_lane.rs",
        enclosing_item: "reference_selected_pool",
        location_or_primitive: "commands::semantic_search::lexical_lane::reference_selected_pool",
        reason: "test-only reference implementation choosing the three rarest trigram postings to compare against the optimized pool; not an agent-visible list",
    },
    ExclusionEntry {
        file: "commands/zoom.rs",
        enclosing_item: "indexed_offsets_avoid_repeated_prefix_scans_at_end_of_large_file",
        location_or_primitive: "commands::zoom indexed offset work-count test",
        reason: "test fixture selecting sample offsets for a work-count assertion; not an agent-visible list",
    },
    // Split-query search (aft_search with both query and pattern). The
    // results list is the search surface's, cut and enveloped by the engine;
    // these items only describe the pattern input.
    ExclusionEntry {
        file: "commands/semantic_search/split_query.rs",
        enclosing_item: "summary_line",
        location_or_primitive: "commands::semantic_search::split_query::SUMMARY_DEFINITION_SITES",
        reason: "the one-line pattern summary names at most three definition sites as a preview; the reply's pattern_summary.definition_files carries the full count, and no result is removed",
    },
    ExclusionEntry {
        file: "commands/semantic_search/split_query.rs",
        enclosing_item: "from_bounded_scan, bounded_scan_groups_lines_by_file_and_keeps_the_bound",
        location_or_primitive: "commands::semantic_search::split_query bounded scan truncation flags",
        reason: "reads (or, in the test, sets) the bounded grep scan's truncation flags to mark the pattern examination capped; nothing is cut here, and the split reply discloses the bound through its budget envelope",
    },
    ExclusionEntry {
        file: "commands/semantic_search/split_query.rs",
        enclosing_item: "selective_definitions, admission_definers, expand_group",
        location_or_primitive: "commands::semantic_search::split_query::MAX_PLACED_DEFINITIONS, MAX_GROUP_EXPANSION",
        reason: "bounds the definitions scored for relevance or carried into the ranking and the alternatives a group expands into; these are ranking inputs, no list is shown to the agent, and the results list is cut and enveloped by the engine",
    },
    // Identifier search in another project (external path): the sweep's
    // results page is the search surface's, registered under its cap reason.
    ExclusionEntry {
        file: "commands/semantic_search/external_exact.rs",
        enclosing_item: "files_with_hits",
        location_or_primitive: "commands::semantic_search::external_exact::sweep skipped_foreign_mounts",
        reason: "counts mount points of other filesystems that the walk does not enter; no list is cut here, and a walk the deadline stops is reported through the sweep's enumeration_stopped and the reply's coverage line",
    },
    ExclusionEntry {
        file: "commands/semantic_search/mod.rs",
        enclosing_item: "count",
        location_or_primitive: "commands::semantic_search external grep coverage line walk_truncated",
        reason: "reads the bounded scan's truncation flag to word the coverage line; nothing is cut here, and the same flag marks the reply incomplete in external_fallback_response",
    },
    ExclusionEntry {
        file: "commands/semantic_search/nearest_names.rs",
        enclosing_item: "nearest_names",
        location_or_primitive: "commands::semantic_search::nearest_names NEAREST_NAME_FILE_LIMIT, NEAREST_NAME_LIMIT",
        reason: "a not-found answer suggests a few nearest names: the scan reads a bounded number of files and keeps the most similar names; these are suggestions for a name that occurs nowhere, not a cut of matching results",
    },
    ExclusionEntry {
        file: "commands/semantic_search/external_pattern.rs",
        enclosing_item: "semantic_results, semantic_index",
        location_or_primitive: "commands::semantic_search::external_pattern::Corpus semantic enumeration",
        reason: "enumerates at most SEMANTIC_ENUMERATION_LIMIT prose candidates before ranking, like the external query-only route; more_available reports the remaining candidates and the engine envelopes the results page. Discovery currently attributes the restricted-visibility method to its preceding semantic_index helper",
    },
    ExclusionEntry {
        file: "commands/inspect.rs",
        enclosing_item: "partial_reason_from_parts",
        location_or_primitive: "commands::inspect::MAX_INSPECT_HEADER_PARTS",
        reason: "the PARTIAL status line previews at most MAX_INSPECT_HEADER_PARTS incomplete parts and says `+N more`; every scanner reason stays in the body and the structured gaps, diagnostic overflow reasons are rendered in the body, and the cap never changes `complete`",
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
