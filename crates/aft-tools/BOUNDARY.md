# Daemon and tools boundary

The daemon may call the tools; the tools do not depend on the daemon. This inventory records the formerly private paths identified by compiling the split, the additional members Rust can check after resolving their owners, and the APIs that replace access to private fields. “Public API (lower export)” uses an already-public lower-crate item rather than widening its tools-side alias. The paths of moved items show their new owner.

| Item path | Kind | Classification | Reason |
| --- | --- | --- | --- |
| `commands::bash_orchestrate::blocking_wait_cap_ms` | function | Public API | Standalone and daemon foreground waits use the same cap. |
| `commands::bash_orchestrate::decide_bash_step` | function | Public API | Both shell paths make the same poll, promotion and completion decisions. |
| `commands::bash_orchestrate::BashStep` | enum (variants exposed) | Public API | Both shell paths match on the decision returned by `decide_bash_step`. |
| `commands::bash_orchestrate::detach_wait_mode_bash` | function | Public API | Both shell paths hand back a running wait after a user message. |
| `commands::bash_orchestrate::foreground_result_response` | function | Public API | Daemon and standalone polls use the same terminal reply. |
| `commands::bash_orchestrate::format_background_handoff_tail` | function | Public API | Both standalone promotion and daemon restart name the task and its available tools. |
| `commands::bash_orchestrate::handback_remote_bash` | function | Public API | Both shell paths hand remote work back after the same blocking wait limit. |
| `commands::bash_orchestrate::kill_deadline_note` | function | Public API | Both shell paths include the active task kill deadline on handoff. |
| `commands::bash_orchestrate::kill_deadline_note_from_parts` | function | Public API | The tools look up the deadline; the daemon can render a previously published snapshot. |
| `commands::bash_orchestrate::poll_bash_status` | function | Public API | Standalone and daemon foreground polls read the same task state. |
| `commands::bash_orchestrate::promote_bash` | function | Public API | Both shell paths promote foreground tasks without killing them. |
| `commands::bash_orchestrate::remote_block_handback_ms` | function | Public API | Both shell paths calculate the same remote blocking wait budget. |
| `commands::bash_orchestrate::REMOTE_INPUT_CAPTURE_WARNING` | const | Public API | Both shell paths reuse the same warning until the runner confirms capture. |
| `commands::bash_orchestrate::remote_job_sentence` | function | Public API | Standalone remote handback and daemon restart describe the same job state. |
| `commands::bash_orchestrate::reply_announces_live_task` | function | Public API | Both shell paths distinguish handoff replies from terminal results. |
| `commands::bash_orchestrate::resolve_foreground_wait_window_ms` | function | Public API | The tools' test and the daemon use the same environment override for foreground waits. |
| `commands::bash_orchestrate::runon_uncaptured_reply` | function | Public API | Both shell paths build the same remote input-capture failure reply. |
| `commands::bash_orchestrate::select_foreground_wait_window_ms` | function | Public API | Both shell paths select the initial foreground wait window. |
| `commands::bash_orchestrate::task_not_found_response` | function | Public API | Both shell pollers produce the same reply for an absent task. |
| `commands::bash_orchestrate::with_capture_warning` | function | Public API | The tools' status replies and daemon handoff replies mark uncaptured remote inputs. |
| `commands::bash_orchestrate::with_runon_capture_state` | function | Public API | Both shell paths preserve the runner's capture and phase information. |
| `commands::bash_orchestrate::worker_kill_deadline_within_handoff_margin` | function | Public API | Daemon shell and tools-side waiters avoid handing back a task about to be killed. |
| `commands::bash_orchestrate::worker_wait_max_ms` | function | Public API | Tools and daemon resolve the same worker wait limit, including the test override. |
| `commands::bash_kill_scope` | module | Public API | Names the existing scope-stop operation and shared recovered-task implementation. |
| `commands::bash_kill_scope::BASH_KILL_SCOPE_OPERATION` | const | Public API | The daemon dispatches the operation and the tools' tests pin its reply name. |
| `commands::bash_kill_scope::handle_bash_kill_scope_with_saved_tasks` | function | Public API | The tools and daemon apply the same scope ownership checks to live and saved tasks. |
| `commands::bash_status::format_unknown_task_message` | function | Public API | Status and the daemon drain reply must give the same missing-task explanation. |
| `commands::bash_status::handle_with_observation_only` | function | Public API | The tools support the normal status call and the daemon supplies the route's observation policy. |
| `commands::bash_watch::DEFAULT_PRIMARY_WATCH_TIMEOUT_MS` | const | Public API | Both the handler and daemon plan primary-session watch waits. |
| `commands::bash_watch::handle_deferred_with_observation_only` | function | Public API | Both the watch handler and daemon rich UI use the same deferred observation mode. |
| `commands::bash_watch::handle_deferred_yieldable` | function | Public API | The watch handler can run directly or receive the daemon's yield signal. |
| `commands::bash_watch::YieldSignal` | struct | Public API | The daemon requests yield and the tools own the wait that observes it. |
| `commands::bash_watch::YieldSignal::request` | method | Public API | The daemon asks the tool-owned signal to stop its wait without touching its atomic flag. |
| `commands::bash_watch::restart` | module | Public API | Names the watch restart operations without exposing their shared lock or fields. |
| `commands::bash_watch::restart::WatchWait` | struct | Public API | The tool owns the initial wait; the daemon settles or recreates it on restart. |
| `commands::bash_watch::restart::WatchWait::new` | constructor | Public API | Normal and restored watches capture timing and caller identity at creation. |
| `commands::bash_watch::restart::WatchWait::ctx` | method | Public API | Daemon settlement reads the original context through a copied handle. |
| `commands::bash_watch::restart::WatchWait::request` | method | Public API | Daemon settlement formats a copy of the original tool request. |
| `commands::bash_watch::restart::WatchWait::checkpoint` | method | Public API | Restart persistence and recovery fixtures use the same checkpoint representation. |
| `commands::bash_watch::restart::reserve_watch_hold` | function | Public API | Atomically reserves the tool-owned wait before the daemon persists it. |
| `commands::bash_watch::restart::clear_watch_hold` | function | Public API | Releases the tool-owned reservation after the daemon fails to persist the hold. |
| `commands::bash_watch::restart::recover_watch_hold` | function | Public API | Restores the wait, timing and durable key in one operation under the original lock. |
| `commands::bash_watch::restart::persist_watch_hold` | function | Public API | Serializes daemon hold persistence against terminal settlement without exposing the lock. |
| `commands::bash_watch::restart::watch_is_held` | function | Public API | The handler skips a held wait and the daemon test verifies a failed reservation leaves it live. |
| `commands::bash_watch::restart::watch_finished` | function | Public API | Tool completion and daemon recovery use one terminal settlement transition. |
| `commands::bash_watch::restart::copy_watch_response` | function | Public API | The tools' terminal settlement and daemon replay test preserve the metadata in the same response copy. |
| `commands::bash_watch::restart::watch_unknown` | function | Public API | Both sides mark a held wait unknown without inventing a new terminal reply. |
| `commands::bash_watch::restart::register` | function | Public API | The daemon installs the settlement callback invoked by the tool-owned wait. |
| `commands::configure::cancel_deferred_configure_maintenance` | function | Public API | Daemon drain and handler tests cancel the same maintenance queue. |
| `commands::configure::handle_cache_recovery` | function | Public API | Configure and daemon standing recovery use one cache recovery procedure. |
| `commands::configure::ignore_rules_load_pending` | function | Public API | The daemon and configure tests determine whether initial ignore rules are still unpublished. |
| `commands::configure::load_ignore_rules_if_unpublished` | function | Public API | The daemon uses the same initial publication operation exercised by configure tests. |
| `commands::edit_match::register_edit_no_match_reporter` | function | Hook | The edit handlers report failed matches without owning the daemon's display scope. |
| `commands::delete_file::handle_delete_deferred_with_restriction` | function | Public API | The dispatcher and daemon both pass a transport-owned delete restriction. |
| `commands::inspect::handle_inspect_deferred_with_reply_deadline` | function | Public API | Normal and daemon inspect calls use the same deferred handler with transport timing. |
| `commands::lsp_navigation::handle_lsp_navigation_deferred_with_reply_deadline` | function | Public API | Normal and daemon navigation share the deferred handler and can supply an earlier deadline. |
| `commands::memory_census::CensusRootRows` | struct | Public API | Daemon health and the census tests share the rendered root-row cache. |
| `commands::memory_census::CensusRootRows::render` | method | Public API | Both consumers reuse root rows across changes that do not affect the rendered fields. |
| `commands::memory_census::render_memory_census_with_roots` | function | Public API | Tools and daemon health render the same census while choosing their own row cache. |
| `commands::semantic_search::first_text_index_wait_needed` | function | Public API | The search handler and daemon both use the same first-index readiness check. |
| `commands::semantic_search::handle_first_index_search_deferred` | function | Public API | The daemon calls the same timed deferred path exercised by the search handler tests. |
| `response_finalize::DRIFTING` | const | Public API | Repeat reminders and the daemon notice catalogue share the registered template ID. |
| `response_finalize::IDENTICAL` | const | Public API | Repeat reminders and the daemon notice catalogue share the registered template ID. |
| `response_finalize::finalize_redisplayed_bash_response` | function | Public API | Daemon redisplay uses the finalization order pinned by the tools' output-cap tests. |
| `response_finalize::finalize_tool_response_with_visibility` | function | Public API | The regular finalizer and daemon can supply the applicable bash visibility. |
| `response_finalize::MAX_RESULT_BYTES` | const | Public API | The read handler and daemon encoder enforce the same wire-size limit. |
| `response_finalize::repeat_instruction` | function | Public API | The shared runner and daemon notice formatter agree on the repeat-breaker instruction. |
| `response_finalize::too_large` | function | Public API | The read handler and daemon wire encoder use the same oversize error. |
| `run_tool_call::ensure_hashline_registration` | function | Public API | The shared runner and daemon shell register the same hashline binding. |
| `run_tool_call::finish_tool_call_response` | function | Public API | The shared runner and daemon deferred path call the same response formatter and finalizer. |
| `run_tool_call::hashline_binding_root` | function | Public API | The shared runner and daemon provider resolve the same hashline binding root. |
| `run_tool_call::PhaseTrace::deferred_search_wait_fields` | method | Public API | The daemon attaches the shared trace's timing to first-index waits, also exercised by tools tests. |
| `run_tool_call::PhaseTrace::launch_restore_wait` | method | Public API | The daemon reads launch blocking time from the same trace used by the tools. |
| `run_tool_call::PhaseTrace::mark_execute_done` | method | Public API | Both call paths mark execution completion on the same timer. |
| `run_tool_call::PhaseTrace::mark_execute_done_after_await` | method | Public API | The daemon records deferred execution completion using the trace logic tested by tools. |
| `run_tool_call::PhaseTrace::mark_provider_read_done` | method | Public API | The daemon marks provider read completion on the shared tools trace. |
| `run_tool_call::PhaseTrace::note_arg_shape` | method | Public API | Both call paths can record the request shape without retaining values. |
| `run_tool_call::PhaseTrace::note_launch_restore_wait` | method | Public API | The daemon records launch restore timing in the shared tools trace. |
| `run_tool_call::PhaseTrace::received_at` | method | Public API | The daemon bases transport timing on the shared tools trace's receipt time. |
| `run_tool_call::prepare_tool_call` | function | Public API | The shared runner and daemon preflight use one request translator. |
| `run_tool_call::PreparedToolCall` | struct | Public API | Both paths carry the translated request and the chosen edit-surface flag. |
| `run_tool_call::PreparedToolCall::into_parts` | method | Public API | Daemon dispatch consumes the translated request and flag without exposing their fields. |
| `run_tool_call::PreparedToolCall::request` | method | Public API | Daemon preflight inspects the translated request without consuming or modifying it. |
| `run_tool_call::RepeatObservation` | struct | Public API | All tool-call paths must record the same repeat-breaker event. |
| `run_tool_call::RepeatObservation::for_agent_call` | constructor | Public API | Tools and daemon capture the same eligible call identity before dispatch. |
| `run_tool_call::RepeatObservation::observe` | method | Public API | Tools and daemon record a repeat event once, on the selected reply. |
| `run_tool_call::resolve_head_edit_mode` | function | Public API | The shared runner and daemon use the same runtime edit-surface selection. |
| `run_tool_call::strip_agent_preview_arg_owned` | function | Public API | Tools and daemon strip the same agent-only argument before dispatch. |
| `run_tool_call::tool_call_arg_shape` | function | Public API | Both paths collect argument shapes without logging individual values. |
| `subc::bash::detach_bash_for_module_drain` | function | Move up | Only the daemon hands back and persists a foreground task when its module drains. |
| `subc::bash::format_detached_call_message` | function | Move up | Only the daemon completes the original call with its restart-detach error. |
| `subc::call_yield::watch_can_hold` | function | Move up | Only daemon yield orchestration asks whether a wait can be held; the tools still own its atomic reservation. |
| `subc_format::BashCodeSearchHint::deliver` | method | Public API | Daemon deferred rendering and standalone tests run the same native reply guidance logic. |
| `subc_format::BashCodeSearchHint::new` | constructor | Public API | Both paths construct the retained hint facts without publishing any fields. |
| `subc_format::civil_from_days` | function | Public API | Tools timestamps and daemon diagnostics use the same Gregorian conversion. |
| `subc_format::CodeSearchNotice` | trait | Handed-down trait | The daemon supplies ordered notice routing and route identity without exposing its transport or budget to tools. |
| `subc_translate::model_boolean` | function | Public API | Both sides coerce supported Boolean forms the same way. |
| `subc_translate::normalize_path_arguments` | function | Public API | The tool translator and daemon read-only checks normalize the same path arguments. |
| `subc_translate::normalize_zoom_aliases` | function | Public API | Tool dispatch and daemon provider preflight recognize the same zoom aliases. |
| `subc_translate::split_search_path_arg` | function | Public API | Local translation and remote dispatch split search paths identically. |
| `subc_translate::supports_tool` | function re-export | Public API | The shared runner and daemon manifest use the same lower-crate tool catalogue. |
| `tool_output` | module | Public API | Names the shared rendered-text cap without exposing its implementation details. |
| `tool_output::cap` | function | Public API | Both paths cap rendered text after all applicable notices and status lines. |
| `tool_output::cap_with_scope` | function | Public API | The shared cap and daemon retention both honor an existing output scope. |
| `tool_output::TEXT_LIMIT` | const | Public API | Wire projection and tool rendering use the same text size cap. |
| `aft_base::clock::ThreadCpuSample` | struct | Public API (lower export) | Daemon write timing and tools tracing use the same sampler without a private re-export. |
| `aft_base::pending_response::DeferredResponseWake` | struct | Public API (lower export) | The daemon uses the lower response wake type instead of a tool-finalizer alias. |
| `aft_engine::blocking_wait` | module | Public API (lower export) | Both sides use the same budget and headroom without the private commands alias. |
| `aft_engine::file_read_exact` | module | Public API (lower export) | Daemon file reading uses the existing descriptor-safe engine API. |
| `aft_engine::data_layer::outline` | module | Public API (lower export) | The daemon binds outline data via the existing engine API rather than the private tools alias. |
| `aft_engine::data_layer::runtime` | module | Public API (lower export) | Daemon and tools share engine checkout lifecycle types and operations. |
| `aft_engine::output_store::OutputScope` | struct | Public API (lower export) | Daemon response retention uses the same engine scope without a private output-cap alias. |

## Test-only tools fixtures

Daemon tests use `tools_test_support` for handler fixtures and observations. It compiles in this crate's unit tests and in library builds that explicitly enable `test-support`; normal builds do not enable it. No daemon state is imported by the tools fixture module.

| Item path | Kind | Classification | Reason |
| --- | --- | --- | --- |
| `tools_test_support` | module | Test support | Names the tools fixtures that daemon tests may use. |
| `tools_test_support::test_app` | fixture | Test support | Builds an app with isolated storage. |
| `tools_test_support::test_context` | fixture | Test support | Builds an isolated handler context with shell handlers installed. |
| `tools_test_support::wait_fixture` | fixture | Test support | Creates a shell wait with the expected worker time limit. |
| `tools_test_support::completed_spawn_for_test` | fixture | Test support | Produces a real spawn reply after observing task completion. |
| `tools_test_support::handle_with_policy` | fixture | Test support | Calls the shell handler with an explicit remote policy. |
| `tools_test_support::rendered_status` | getter | Test support | Reads one task status in the format received by an agent. |
| `tools_test_support::external_search_fixture` | fixture | Test support | Creates an external checkout and publishes its text index. |
| `tools_test_support::direct_external_search_for_test` | fixture | Test support | Gives the daemon hook test a direct handler reply to compare against. |
| `tools_test_support::configure_unit_records_for_root` | getter | Test support | Copies configure-tail measurements for one fixture root. |
| `tools_test_support::ConfigureUnitRecord` | struct | Test support | Holds one stage observation without exposing its fields across the boundary. |
| `tools_test_support::ConfigureUnitRecord::stage` | getter | Test support | Reads the completed configure stage's label. |
| `tools_test_support::ConfigureUnitRecord::finished_at` | getter | Test support | Reads the recorded completion time for that stage. |
| `tools_test_support::gate_configure_commit_for_test` | fixture | Test support | Holds one configure call between configuration publication and artifact updates. |
| `tools_test_support::gate_configure_tail_stage_for_test` | fixture | Test support | Holds a selected configure-tail stage for one root. |
| `tools_test_support::ConfigureTailStageGateGuard` | struct | Test support | Removes the stage gate on drop. |
| `tools_test_support::ConfigureTailStageGateGuard::join_storage_sweep_worker` | fixture | Test support | Reaps a fixture worker before its temporary directories are removed. |
| `tools_test_support::install_delete_gate_for_test` | fixture | Test support | Holds a recursive delete after backup planning. |
| `tools_test_support::deferred_inspect_test_lock` | fixture | Test support | Serializes tests sharing the deferred inspect gate. |
| `tools_test_support::deferred_inspect_root_count_for_test` | getter | Test support | Checks whether inspect still holds a permit for the fixture root. |
| `tools_test_support::install_deferred_inspect_body_gate_for_test` | fixture | Test support | Holds the next deferred inspect at its request body. |
| `tools_test_support::install_deferred_inspect_body_gate_for_request_for_test` | fixture | Test support | Holds only the specified deferred inspect request. |
| `tools_test_support::install_deferred_inspect_stat_gate_for_test` | fixture | Test support | Holds inspect at its stat path without running unrelated stages. |
| `tools_test_support::deferred_navigation_test_lock` | fixture | Test support | Serializes tests sharing the deferred navigation gate. |
| `tools_test_support::deferred_navigation_worker_count_for_test` | getter | Test support | Counts outstanding navigation workers for cancellation and drain tests. |
| `tools_test_support::install_deferred_navigation_gate_for_test` | fixture | Test support | Pauses the next navigation worker before calling its handler. |
| `tools_test_support::take_deferred_navigation_seen_restrict_for_test` | getter | Test support | Reads and clears the worker's recorded restriction flag. |
| `tools_test_support::glob_root_files_for_test` | fixture | Test support | Observes scoped glob discovery and its visited-entry count. |
| `tools_test_support::discover_outline_paths_for_test` | fixture | Test support | Observes scoped outline discovery without the request's earlier path checks. |
| `tools_test_support::with_secret_fetch_for_test` | fixture | Test support | Installs a fake vault and fetch options while the handler runs. |
| `tools_test_support::with_secret_fetch_test_hooks` | fixture | Test support | Configures a fake fetch and lets daemon tests observe a private document before deletion. |
| `tools_test_support::text_index_is_ready` | getter | Test support | Checks readiness without exposing the handler's text-index wrapper. |
| `tools_test_support::watch_restart_registered` | getter | Test support | Checks whether daemon startup installed the tools' restart callback. |
| `tools_test_support::install_watch_wait_fixture` | fixture | Test support | Installs a waiting watch without exposing the restart state lock. |
| `tools_test_support::ranked_oversized_first_response` | fixture | Test support | Renders a large first hit and small second hit without exposing ranked result fields. |

## Test identities

[`test-ledger/moves.tsv`](test-ledger/moves.tsv) lists changed identities relative to `4dc94a5f6ad9cd0c8bf4159b15bb6dc85ab3bc52`, before the daemon boundary was prepared. Unlisted identities map to themselves with unchanged multiplicity. [`test-ledger/additions.tsv`](test-ledger/additions.tsv) lists new tests separately. An identity consists of the package, `kind:name` of the Cargo target, and the full test path. The last three columns are the expected occurrences on Linux, macOS and Windows, in that order; zero means the test is not compiled on that platform. The platform counts come from native listings, not from reading source-level platform attributes.

Pass the base and current native tuple ledgers captured using `cargo test --workspace --features wasm-grammars,test-hooks -- --list` and workspace metadata to `python3 crates/aft/test-ledger/compare.py <platform> <base-tuples.tsv.gz> <current-tuples.tsv.gz>`. The comparator checks the old counts before applying the mapping, then compares both directions against the current listing.
