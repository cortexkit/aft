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
                predicate_name: "bounded_walk_search_envelope, fallback_search_envelope, execute_fallback_mode, ExternalFallbackBody, external_fallback_response, external_exact::sweep, walk_truncated",
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
                predicate_name: "SearchTrailer::shared_envelope_projection, StopState::S1MoreAtDepth, StopState::S2Exhausted, more_available, handle_external_semantic_or_hybrid_search, handle_external_grep_search, handle_semantic_or_hybrid_search, handle_grep_search, from_matches, run_engine_ranking, blast_radius_annotation_for_result, enrich_snippets_from_source_reference, enrich_snippets_from_source_with_context, read_bounded_snippet_line, truncate_chars, split_semantic_results, handle_split_search, rank_hits",
            },
        ],
    },
    SurfaceEntry {
        command: "grep",
        mode: "",
        list_id: "payload.matches",
        unit: Unit::Rows,
        narrow: &["offset", "path", "include"],
        reasons: &[
            ReasonEntry {
                reason: Reason::Walk,
                kind: ReasonKind::Bounding,
                predicate_name: "handle_grep, handle_grep_inner, grep_result, grep_result_bytes, walk_truncated, skipped_foreign_mounts",
            },
            ReasonEntry {
                reason: Reason::Cap,
                kind: ReasonKind::Selecting,
                predicate_name: "DEFAULT_MAX_RESULTS, GREP_MAX_OUTPUT_BYTES, MAX_DISPLAY_MATCHES_PER_FILE, FIRST_PAGE_ROWS, FIRST_PAGE_BYTES, FIRST_PAGE_ROUNDS, WHOLE_RESULT_ROWS, WHOLE_RESULT_BYTES, SUMMARY_FILES_LISTED, SUMMARY_FOLDERS_LISTED, handle_grep, handle_grep_inner, format_grep_text, grep_page_output, round_robin_order, files_first_summary, rendered_grep_match_count",
            },
            // Lines in lockfiles, generated and minified files, logs and
            // JSON data files are counted per file instead of printed, unless
            // the call names them; past COUNTED_ENTRIES_LISTED entries the
            // counted-only block folds the rest into one row per kind and
            // marker.
            ReasonEntry {
                reason: Reason::Kind,
                kind: ReasonKind::Selecting,
                predicate_name: "HANDFUL_ROWS, HANDFUL_BYTES, COUNTED_ENTRIES_LISTED, grep_page_output, counted_entries, include_targets",
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
                reason: Reason::TimeLimit,
                kind: ReasonKind::Bounding,
                predicate_name: "walk_time_limit_reached, walk_files_unbounded, bounded_fallback_walk_files_with_limits_target",
            },
            ReasonEntry {
                reason: Reason::Walk,
                kind: ReasonKind::Bounding,
                predicate_name: "GlobDiscovery, handle_glob, handle_glob_inner, fallback_glob, glob_root, discover_glob_root, named_ignored_glob_walk_stops_at_cap_and_deadline, walk_truncated, skipped_foreign_mounts",
            },
            ReasonEntry {
                reason: Reason::Cap,
                kind: ReasonKind::Selecting,
                predicate_name: "DEFAULT_MAX_RESULTS, MAX_DISPLAY_DIRECTORIES, MAX_DISPLAY_FILES_PER_DIRECTORY, handle_glob, handle_glob_inner, format_glob_text, rendered_glob_file_count",
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
                reason: Reason::TimeLimit,
                kind: ReasonKind::Bounding,
                predicate_name: "outline_batch_deadline_reached, stop_at_deadline, take_back_last_file, build_outline_structure_envelope",
            },
            ReasonEntry {
                reason: Reason::Budget,
                kind: ReasonKind::Selecting,
                predicate_name: "MAX_OUTPUT_BYTES, discover_outline_files, handle_outline_files_mode, MultiFileTreeWriter, requested_outline_files, budget_rollups_present",
            },
            ReasonEntry {
                reason: Reason::Walk,
                kind: ReasonKind::Bounding,
                predicate_name: "OutlineFileDiscovery, discover_outline_files_with_options, discover_outline_files_unbounded, collect_outline_files_with_device_lookup, collect_outline_files_breadth_first_with_device_lookup, outline_walk_skips_and_reports_injected_foreign_mount, ITERATIONS, collection_truncated, walk_truncated, skipped_foreign_mounts",
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
            predicate_name: "details_for, generated_details_for, test_only_details_for, uncovered_files_details_for, topk_limiting, write_skipped_files, filter_payload_for_scope, SKIPPED_FILES_LIMIT",
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
    SurfaceEntry { command: "bash", mode: "changed_files", list_id: "bash.changed_files", unit: Unit::Files, narrow: &[], reasons: &[ReasonEntry { reason: Reason::Cap, kind: ReasonKind::Selecting, predicate_name: "render" }, ReasonEntry { reason: Reason::TimeLimit, kind: ReasonKind::Bounding, predicate_name: "render" }] },
    SurfaceEntry { command: "bash", mode: "diff_lines", list_id: "bash.diff_lines", unit: Unit::Lines, narrow: &[], reasons: &[ReasonEntry { reason: Reason::Cap, kind: ReasonKind::Selecting, predicate_name: "diff_file" }] },
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
        file: "bash_permissions/scan.rs",
        enclosing_item: "top_level_directory_targets, MAX_CWD_PATHS, insert",
        location_or_primitive: "MAX_CWD_PATHS",
        reason: "internal cwd analysis retains at most eight folder/mode states per exit-status branch; exceeding that bound makes the folder unknown rather than removing alternatives, and no response list is shortened",
    },
    ExclusionEntry {
        file: "standalone_input.rs",
        enclosing_item: "read_bounded_line, room, push",
        location_or_primitive: "MAX_LINE_BYTES / MAX_RETAINED_BYTES / MAX_RETAINED_ITEMS / MAX_HELD_BYTES / MAX_HELD_ITEMS",
        reason: "transport admission and allocation bounds, not result-list cuts; oversized frames and excess held requests receive explicit errors, retained input keeps its budget charge across queue moves",
    },
    ExclusionEntry {
        file: "cli/sandbox_launch/landlock_backend.rs",
        enclosing_item: "session_task_read_paths",
        location_or_primitive: "tasks.truncate(MAX_SESSION_READ_TASKS)",
        reason: "internal Landlock read-grant snapshot; tasks beyond the 500-entry budget stay unreadable without a broader grant, not an agent-visible list",
    },
    ExclusionEntry {
        file: "gh_shim/read_contract.rs",
        enclosing_item: "validate, validate_path",
        location_or_primitive: "MAX_REPLY_BYTES job ranges and per_page <= 100",
        reason: "closed GitHub read wire validation refuses oversized ranges and page sizes; it does not truncate an AFT result list",
    },
    ExclusionEntry {
        file: "gh_shim/read_executor.rs",
        enclosing_item: "decode_reply",
        location_or_primitive: "REPLY_BYTES payload ceiling and returned byte range validation",
        reason: "the GitHub read wire refuses oversized or malformed replies rather than silently cutting a response list",
    },
    ExclusionEntry {
        file: "gh_shim/read_render.rs",
        enclosing_item: "checked, fetch, stream_log",
        location_or_primitive: "REPLY_BYTES and bounded JobLog byte windows",
        reason: "governed read transport bound, not an AFT list surface; oversized replies refuse and complete logs stream through all byte windows",
    },
    ExclusionEntry {
        file: "gh_shim/read_render.rs",
        enclosing_item: "list, search, native",
        location_or_primitive: "per_page 100, items.iter().take(remaining), items.iter().take(plan.limit() - rows.len()) and native gh --limit selection",
        reason: "REST API paging and gh-compatible explicit output limits, not an AFT tool result list; over-return is a fetch error",
    },
    ExclusionEntry {
        file: "gh_shim/worker_reads.rs",
        enclosing_item: "limit, search_query",
        location_or_primitive: "native gh defaults and 1000-result search ceiling",
        reason: "gh-compatible CLI output limits and GitHub search refusal, not an AFT list surface",
    },
    ExclusionEntry {
        file: "lsp/manager.rs",
        enclosing_item: "walk_applicable_area_uncached",
        location_or_primitive: "visited >= entry_limit",
        reason: "internal applicability discovery, not a response list; exhausting the iterator budget returns an explicit incomplete-walk error instead of certifying a partial server set",
    },
    ExclusionEntry {
        file: "runtime_drain.rs",
        enclosing_item: "collect_mutation_paths",
        location_or_primitive: "budget.exhausted()",
        reason: "bounded internal mutation-index discovery, not a response list; ignored directories are pruned, examined counts are logged, and unfinished work schedules a background corpus replacement with the index marked not ready",
    },
    ExclusionEntry {
        file: "backup.rs",
        enclosing_item: "read_session_disk_index_with_budget, merge_disk_stack_heads",
        location_or_primitive: "MAX_SESSION_DISCOVERY_ENTRIES, examined >= entry_limit, BackupIoBudget::check, ROLLBACK_CLEANUP_ALLOWANCE",
        reason: "internal durable undo-stack discovery stops at its entry or deadline budget and returns an error, never a partial path list or false no-history result; rollback uses one independent bounded cleanup allowance after cancellation, and per-file undo avoids the session scan",
    },
    ExclusionEntry {
        file: "runtime_drain.rs",
        enclosing_item: "drop",
        location_or_primitive: "budget.exhausted()",
        reason: "bounded internal mutation-index refresh, not a response list; refreshed counts are logged and budget exhaustion schedules one background corpus replacement instead of certifying stale search answers as complete",
    },
    ExclusionEntry {
        file: "commands/configure.rs",
        enclosing_item: "join_storage_sweep_worker",
        location_or_primitive: "Option .take()",
        reason: "test-only handoff of the held storage sweep worker's join handle out of its gate, not a list",
    },
    ExclusionEntry {
        file: "commands/configure.rs",
        enclosing_item: "view_working_tree_refresh_path",
        location_or_primitive: "max_examined .take()",
        reason: "startup verification of persisted view bytes, not a result list; reaching the iterator budget schedules a full background publication and refuses stale graph answers rather than treating partial verification as complete",
    },
    ExclusionEntry {
        file: "commands/configure.rs",
        enclosing_item: "view_git_changed_paths",
        location_or_primitive: "max_examined .take()",
        reason: "bounded startup dirty-path discovery, not a result list; truncated subprocess output or reaching the iterator budget forces full view publication rather than certifying the checkout as clean",
    },
    ExclusionEntry {
        file: "context.rs",
        enclosing_item: "should_emit",
        location_or_primitive: "256-entry status-bar session history / pop_front",
        reason: "bounded deduplication bookkeeping, not a result list; evicting an idle session only repeats its next bar and never suppresses a changed value",
    },
    ExclusionEntry {
        file: "edit.rs",
        enclosing_item: "drop, notify_full_validation_start",
        location_or_primitive: "FULL_VALIDATION_START_HOOK Option::take",
        reason: "consumes or clears a test-only callback; Option::take removes one optional value, not entries from an agent-visible list",
    },
    ExclusionEntry {
        file: "edit.rs",
        enclosing_item: "foreign_group",
        location_or_primitive: "groups.truncate(count.max(0) as usize)",
        reason: "test helper shrinking a getgroups buffer to the count the OS returned; group ids for a fixture, never an agent-visible list",
    },
    ExclusionEntry {
        file: "edit.rs",
        enclosing_item: "read_sized",
        location_or_primitive: "buffer.truncate(read_size as usize)",
        reason: "shrinks a byte buffer to the size the OS reported for an extended-attribute name list or value; raw metadata copied to the replacement file, never an agent-visible list",
    },
    ExclusionEntry {
        file: "edit.rs",
        enclosing_item: "REFORMATTED_EXCERPT_MAX_LINES, REFORMATTED_EXCERPT_MAX_BYTES, compute_reformatted_excerpt, excerpt_caps_bound_owned_line_construction",
        location_or_primitive: "REFORMATTED_EXCERPT_MAX_",
        reason: "bounds an inline post-format source excerpt, not a paginated list; oversized excerpts retain the existing extensive marker instead of returning partial source text",
    },
    ExclusionEntry {
        file: "subc_format.rs",
        enclosing_item: "format_ast_search_capture",
        location_or_primitive: "AST_CAPTURE_INLINE_CHAR_LIMIT .chars().take()",
        reason: "long or multiline AST capture values are referenced by their line range in the matched excerpt with an ellipsized first-line character preview; no match or capture entries are omitted, and the complete values remain in the structured response",
    },
    ExclusionEntry {
        file: "subc_format.rs",
        enclosing_item: "append_zoom_annotation_calls",
        location_or_primitive: "ZOOM_CALL_ANNOTATION_LIMIT .take()",
        reason: "aft_zoom displays at most 15 file-local call references per annotation section and counts omitted references with +N more; all annotation entries remain in the structured response",
    },
    ExclusionEntry {
        file: "bash_background/registry.rs",
        enclosing_item: "format_background_slot_refusal",
        location_or_primitive: "SLOT_REFUSAL_MAX_ROWS / SLOT_REFUSAL_COMMAND_CHARS .take()",
        reason: "a background-launch error previews at most eight of the caller's slot holders, counts omitted own holders and other sessions' holders, and shortens command labels with an ellipsis; this is actionable refusal context, not a paginated task-result list",
    },
    ExclusionEntry {
        file: "subc/health.rs",
        enclosing_item: "budget_health_metrics",
        location_or_primitive: "storage_retention map removal and v2_family_sweeps pop under HEALTH_METRICS_BUDGET_BYTES",
        reason: "management health detail, not an agent result list; sweep rows yield before root diagnostics, omitted sweep and retention rows are counted exactly and aggregate counters remain present",
    },
    ExclusionEntry {
        file: "gc/family.rs",
        enclosing_item: "SweepStatsStore::update",
        location_or_primitive: "SWEEP_STATS_FAMILY_LIMIT / pop_front",
        reason: "observational health telemetry retains the latest 64 family details, not a paginated result list; cap evictions and retired-family removals are disclosed separately",
    },
    ExclusionEntry {
        file: "status_push.rs",
        enclosing_item: "project",
        location_or_primitive: "REFUSAL_DETAILS .take()",
        reason: "root-owned status notification detail, not an agent result list; exact refusal totals, omitted and unattributed counts remain present with an explicit status read pointer",
    },
    ExclusionEntry {
        file: "status_push.rs",
        enclosing_item: "bounded_body, scalar, summary",
        location_or_primitive: "BODY_MAX_BYTES count-only wire summary and bounded scalar strings",
        reason: "status notifications are not paginated results; deterministic compact summaries retain consumer scalar state and exact list/refusal totals with an explicit status read pointer",
    },
    ExclusionEntry {
        file: "subc/push.rs",
        enclosing_item: "drain_turn",
        location_or_primitive: "STATUS_FANOUT_TURN_BYTES",
        reason: "per-root status wire scheduling budget; remaining route deliveries retain the latest state and resume on later turns without omitting recipients",
    },
    ExclusionEntry {
        file: "bash_background/registry.rs",
        enclosing_item: "cleanup_finished_at_with, gc_harness_stores",
        location_or_primitive: "GC_ENTRY_BUDGET maintenance queue and namespace iterator",
        reason: "internal housekeeping advances bounded round-robin queues and resumable directory iterators; retained work is revisited on later passes, not truncated from an agent-visible list",
    },
    ExclusionEntry {
        file: "sandbox_spawn/task_temp.rs",
        enclosing_item: "sweep_stale",
        location_or_primitive: "SWEEP_BUDGET directory iterator advancement",
        reason: "internal crash cleanup resumes the same iterator on later task launches; the budget bounds maintenance work and does not cut an agent-visible list",
    },
    ExclusionEntry {
        file: "bash_background/gc_cursor.rs",
        enclosing_item: "next_batch",
        location_or_primitive: "GC_ENTRY_BUDGET directory iterator advancement",
        reason: "internal persisted-task and quarantine retention scans resume the same iterator next pass; the cap bounds inspection work, not returned records",
    },
    ExclusionEntry {
        file: "bash_background/remote.rs",
        enclosing_item: "append_executor_environment_disclosure",
        location_or_primitive: "executor environment name preview min(10)",
        reason: "supplemental execution note names at most ten filtered environment variables and explicitly counts the rest with +N more; the complete executor report remains in persisted task metadata, not a paginated tool-result list",
    },
    ExclusionEntry {
        file: "subc/bind_log.rs",
        location_or_primitive: "consumer_capabilities preview (eight-entry slice cap)",
        reason: "Bounds RouteBind INFO logging only; does not cut a product result list.",
        enclosing_item: "consumer_capabilities",
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
        file: "commands/file_read_exact.rs",
        enclosing_item: "bounded_bytes",
        location_or_primitive: "exact binary read byte ceiling",
        reason: "the module operation refuses oversized files after at most one lookahead byte; no truncated list or partial file is returned",
    },
    ExclusionEntry {
        file: "subc/perf_sweep.rs",
        enclosing_item: "parse_log",
        location_or_primitive: "bounded log iterator and take(8) metric previews",
        reason: "flow-only telemetry stops at byte or line caps and reports truncated plus scanned counts; top tool and WARN previews disclose tools_total and warn_shapes_total, not a paginated agent list",
    },
    ExclusionEntry {
        file: "subc/perf_sweep.rs",
        enclosing_item: "summarize_profile, child_output, ChildCapture::drain",
        location_or_primitive: "top eight threads, first stack, leaf frame and child-output ceilings",
        reason: "flow-only profile summary preserves the sweep script's preview limits with threads_total; bounded child stdout is refused as a named gap rather than returned partially, and a marked bounded stderr tail preserves the failure cause",
    },
    ExclusionEntry {
        file: "url_fetch.rs",
        enclosing_item: "read_response_body, download_to_cache",
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
        enclosing_item: "diagnose_scope_counts, diagnose_scope_counts_unbounded, bounded_fallback_walk_files_with_limits_target, walk_files_unbounded, walk_each_file_unbounded",
        location_or_primitive: "MAX_FALLBACK_WALK_FILES / FALLBACK_WALK_BUDGET",
        reason: "bounded filesystem fallback and empty-scope exclusion probe; ignored directories are pruned and counted once, and an exhausted probe reports unknown/incomplete rather than claiming an empty filesystem",
    },
    ExclusionEntry {
        file: "inspect/diagnostics_category.rs",
        enclosing_item: "ScopeWalkBudget, scoped_coverage_candidates_with_budget",
        location_or_primitive: "200_000 entry limit and scoped walk deadline",
        reason: "internal scoped-file inventory, not a paginated findings list; budget rejection is retained separately from iterator exhaustion and adds an analysis_incomplete gap with unknown diagnostic totals",
    },
    ExclusionEntry {
        file: "commands/inspect.rs",
        enclosing_item: "scoped_in_root_file_count, build_inspect_payload_with_offset",
        location_or_primitive: "ScopeWalkBudget scoped file count",
        reason: "internal corpus census, not a paginated findings list; an incomplete walk reports a named gap, marks scope_files_complete false, and renders the count as a lower bound",
    },
    ExclusionEntry {
        file: "root_cache.rs",
        enclosing_item: "sweep_all_read_markers_pinned, sweep_marker_generation, read_marker_protection, marker_file_is_protected",
        location_or_primitive: "shared repair traversal budget and read-marker byte limit",
        reason: "internal marker liveness and cleanup, not result lists; incomplete or oversized marker reads protect the cache rather than authorize deletion",
    },
    ExclusionEntry {
        file: "bash_background/persistence.rs",
        enclosing_item: "RepairBudget, unix_names, repair_names, repair_identities, repair_measure, repair_remove_contents, windows_repair_names, repair_descendant, repair_validate_descendants",
        location_or_primitive: "repair traversal depth, entry and deadline bounds",
        reason: "internal cache repair and retention walks, not result lists; an incomplete identity census or inventory refuses deletion and interrupted removal reports partial progress",
    },
    ExclusionEntry {
        file: "cache_location.rs",
        enclosing_item: "candidates_and_durable, deletion_protections, ensure_cache_idle",
        location_or_primitive: "RepairBudget",
        reason: "internal cleanup preflight, not result lists; exhausting the traversal budget refuses all deletion rather than accepting incomplete protection or lock inventories",
    },
    ExclusionEntry {
        file: "storage_retention.rs",
        enclosing_item: "bounded_dirs, traversal_batch_filtered, next_batch, validate_tree, due_first, orphan_first, run_pass_with_budget, prune_bindings, retirement_members, write_retirement_witness, verify_retirement_witness",
        location_or_primitive: "DUE_FIRST_ENTRIES, PASS_ENTRIES, TREE_ENTRIES, HISTORY_ENTRIES, take",
        reason: "internal disk-maintenance walks, not tool-result lists; bounded lexical batches persist only processed names, incomplete protection snapshots refuse deletion, and health reports examined, removed, remaining vanished views and stopped-early counts",
    },
    ExclusionEntry {
        file: "views/registry.rs",
        enclosing_item: "members_bounded, read_members_limited",
        location_or_primitive: "SQL LIMIT",
        reason: "internal family membership census, not an agent-visible list; an incomplete membership snapshot aborts background marking rather than deleting unmarked content",
    },
    ExclusionEntry {
        file: "views/assembly_seed.rs",
        enclosing_item: "worktree_roots, read_worktree_child, read_worktree_roots",
        location_or_primitive: "take(MAX_BYTES + 1), take(65_536), MAX_WORKTREES",
        reason: "optional sibling donor discovery bounds Git topology bytes, records and worktrees at their iterators; an incomplete or malformed enumeration disables reuse rather than claiming a freshest donor",
    },
    ExclusionEntry {
        file: "views/derived_index.rs",
        enclosing_item: "candidates, record",
        location_or_primitive: "SQL LIMIT CANDIDATES_PER_KIND, LIMIT RETAINED_ROWS",
        reason: "internal donor hints for derived-database seeding, not an agent-visible list; a candidate outside the bound or a trimmed row only means a seedless publication falls back to the worktree census or a cold build",
    },
    ExclusionEntry {
        file: "views/generation.rs",
        enclosing_item: "generation_directory_entries, blob_references_by_generation, sweep_generations_bounded, remove_generation_files",
        location_or_primitive: "take(limit.saturating_add(1)), take(GENERATION_DIRECTORY_LIMIT)",
        reason: "internal generation/protection enumeration; ownership or blob-mark overflow aborts before deletion, while an incomplete temporary-file deletion pass only leaves garbage for a later pass",
    },
    ExclusionEntry {
        file: "gc/mod.rs",
        enclosing_item: "sweep_bounded, mark_live_assembly_pins, sweep_plane",
        location_or_primitive: "take(8193), take(4097), LIMIT 4096",
        reason: "internal mark-and-sweep budgets; incomplete marks refuse deletion and a SQL keyset cursor resumes bounded deletion; row/deletion counts and compaction deferrals are reported",
    },
    ExclusionEntry {
        file: "root_cache.rs",
        enclosing_item: "read_marker_protection",
        location_or_primitive: "take(4097)",
        reason: "internal reader-protection census; an unread marker or iterator overflow is protected, never permission to unlink a generation",
    },
    ExclusionEntry {
        file: "callgraph_store/mod.rs",
        enclosing_item: "gc_old_generations",
        location_or_primitive: "take(4096)",
        reason: "internal generation deletion batch, not a query result list; the current pointer and protected readers are checked independently for every candidate",
    },
    ExclusionEntry {
        file: "lsp/completed_rust_check.rs",
        enclosing_item: "sweep_records",
        location_or_primitive: "RECORD_SCAN_LIMIT, MAX_RECORDS",
        reason: "background disk-cache maintenance, not an agent-visible list; the scan counts directory entries inside the iterator loop and logs examined, removed and completion counts; a bounded scan refuses a new record until it can certify the 64-record cap",
    },
    ExclusionEntry {
        file: "subc/tool_provider.rs",
        enclosing_item: "uncertain_settlement_frame",
        location_or_primitive: "SETTLEMENT_RESULT_EXCERPT_BYTES",
        reason: "bounded diagnostic byte excerpt of an already formatted terminal, not a result-list cutoff; original_result_truncated explicitly discloses omitted bytes and no replay guarantee is claimed",
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
        enclosing_item: "refresh_legacy_git_hooks_until",
        location_or_primitive: "legacy hook directory and file byte take",
        reason: "internal cache-maintenance enumeration and input-byte safety bounds, not an agent-visible list; the iterator caps new verification work, preserves verified progress on a bounded scan, and logs examined and rewritten counts plus whether its scan was bounded",
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
    ExclusionEntry {
        file: "commands/glob.rs",
        enclosing_item: "exact_file_glob_with_ready_index_matches_basename_without_widening_scope, exact_file_glob_fallback_matches_basename_without_widening_scope",
        location_or_primitive: "test walk_truncated assertions",
        reason: "test-only assertions on walk_truncated for indexed and fallback exact-file discovery; they do not cut a list or produce a tool response",
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
        file: "bash_background/registry.rs",
        enclosing_item: "read_artifact_page",
        location_or_primitive: "ARTIFACT_PAGE_BYTES",
        reason: "raw artifact transport is paged in bytes, not output lines or list records; callers continue at the actual returned byte offset and has_more discloses a captured backlog rather than claiming exhaustion",
    },
    ExclusionEntry {
        file: "commands/bash_status.rs",
        enclosing_item: "read_status_artifact_page",
        location_or_primitive: "ARTIFACT_PAGE_BYTES",
        reason: "store-unavailable status reads use the same bounded byte pages as registered tasks; next_offset and has_more describe the remaining artifact rather than a list truncation envelope",
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
        file: "watcher_filter.rs",
        enclosing_item: "derive_exclusion_plan, refresh_touched",
        location_or_primitive: "WATCHER_EXCLUSION_LIMIT kernel exclusion slots",
        reason: "internal watcher exclusion budget, not an agent-visible result list; paths without a kernel exclusion slot remain rejected at the walk boundary",
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
        file: "search_index.rs",
        enclosing_item: "git_diff_within_path_limit",
        location_or_primitive: "WARM_GIT_DIFF_PATH_LIMIT",
        reason: "bounds the union of warm Git-diff and disk-stale paths; over-limit refreshes are rejected before content reads and configure falls back to a full cold rebuild, so no agent-visible list is truncated",
    },
    ExclusionEntry {
        file: "search_index.rs",
        enclosing_item: "verify_file_mtimes_with_budget",
        location_or_primitive: "changed_paths.len() > limit",
        reason: "admission budget for disk-only changes and Git-diff paths; an over-budget refresh returns a cache miss for a permitted full rebuild, not a truncated agent-visible list",
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
        reason: "the PARTIAL status line shows every explicitly requested cause first, then previews unrequested causes up to MAX_INSPECT_HEADER_PARTS total parts and says `+N more`; every scanner reason stays in the body and the structured gaps, diagnostic overflow reasons are rendered in the body, and the cap never changes `complete`",
    },
    ExclusionEntry {
        file: "commands/inspect.rs",
        enclosing_item: "incomplete_analysis_reason",
        location_or_primitive: "commands::inspect::MAX_PARSE_FAILURE_FILES",
        reason: "an incomplete-analysis explanation previews five parse-failure paths with their parse reasons and says `+N more`; this is a coverage gap, not a findings page. The full failure count and structured parse_errors stay visible and scope can narrow the affected files; the preview never changes completeness",
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
