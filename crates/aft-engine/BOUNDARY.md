# Engine boundary

Modules with an engine compatibility path keep that path for tool, daemon and facade callers. `#[doc(hidden)]` cross-crate implementation items are not new tool APIs.

| Item | Kind | Owning crate | Reason |
|---|---|---|---|
| `executor::{Executor, ExecutorContext, ExecutorJob, RepeatableJob, …}` | existing lower owner | `aft-base` | The generic executor and its context trait are already lower-owned. |
| `executor::Executor<C = AppContext>` and `context::{Executor, ExecutorJob, RepeatableJob, DispatchLoopLiveness}` | compatibility aliases | `aft-engine` | Compose the generic executor with the engine context without changing caller types. |
| `storage_path::cache_artifact_root` | existing lower owner | `aft-base` | Keep the artifact-root policy shared between index and shell. |
| `bash_background::{cache_artifact_root, storage_dir}` | compatibility re-exports | `aft-engine` | Preserve the established task-module paths. |
| `task_entry::safe_task_entry` | item moved down | `aft-base` | Recursive readers share one task entry policy. |
| `bash_background::registry::safe_task_entry` | compatibility re-export | `aft-engine` | Preserve the task registry path. |
| `process::{GroupListing, ProcessIdentity, LiveDescendant, LiveDescendantsUnavailable}` | items moved down or re-exported | `aft-base` | LSP and shell share one process-group inspection vocabulary. |
| `process::{live_process_group_members, terminate_process, terminate_pgid, terminate_pid, is_process_alive, is_recorded_process_alive, start_new_session}` | items moved down or re-exported | `aft-base` | LSP and shell share process termination, liveness and child session setup. |
| `process::{TERMINATE_GRACE, LIVE_DESCENDANT_CAP, LIVE_DESCENDANT_NAME_CAP, OMITTED_ARGUMENTS_MARKER, GROUP_IDENTITY_CAP}` | items moved down | `aft-base` | Keep constants beside the process helpers that enforce them. |
| `bash_background::process` | compatibility re-export | `aft-engine` | Preserve the old helper paths; shell payload handling stays in the engine. |
| `memory` | existing lower owner | `aft-base` | The accounting implementation was already lower-owned. |
| `memory::profile` | module moved down | `aft-base` | Indexing and caches share the same process profile, allocator policy and admission state. |
| `memory::profile::publish_with_notification` | port | `aft-base` | Invert `memory → data_layer`: call the synchronous notifier after storing the profile and inside the publication mutex. |
| `memory::profile::{publish, current, current_effective, effective, EffectiveMemory, MemoryMode, MemoryProfile, admitted}` | compatibility wrapper and re-exports | `aft-engine` | Keep the old API; publish wires in index limit refresh. |
| `tool_call_result::ToolCallResult` | module moved down | `aft-base` | Response data does not need the composed engine. |
| `tool_call_result` | compatibility re-export | `aft-engine` | Preserve the response type's prior path. |
| `root_identity` and `root_identity::key` | modules moved down | `aft-db` | Root identity reads durable standing-root pins and shares its identity state with scoped keys. |
| `scoped_key` | module moved down | `aft-db` | Scoped resolution uses the same persisted root identity. |
| `root_identity` and `scoped_key` | compatibility re-exports | `aft-engine` | Keep root-key and identity paths available to existing callers. |
| `persisted_format` and `reader_floor` | modules moved down | `aft-db` | Persistent format and reader floors use the database schema and share one refusal registry. |
| `persisted_format` and `reader_floor` | compatibility re-exports | `aft-engine` | Preserve prior persistent policy paths. |
| `build_breaker` | module moved down | `aft-db` | The build breaker opens a tracked SQLite database. |
| `build_breaker` | compatibility re-export | `aft-engine` | Keep breaker paths unchanged. |
| `standing_roots` | module moved down | `aft-db` | Standing-root reconciliation owns database state. |
| `artifact_publish_epoch::ArtifactPublishEpoch` | item moved down | `aft-db` | Standing-root publication lease and epoch. |
| `standing_publication_lease::StandingPublicationLease` | port | `aft-db` | Standing publication verifies the engine's writer lease under the publish epoch; `Arc` forwards verification for existing callers. |
| `impl StandingPublicationLease for root_cache::WriterLease` | compatibility adapter | `aft-engine` | Use the real lease without moving acquisition or its registries down. |
| `root_cache::ArtifactPublishEpoch` and `standing_roots` | compatibility re-exports | `aft-engine` | Preserve old epoch and standing-root paths. |
| `read_fence::{Fence, capture, inherit, with_remote_mutation, remote_mutation, ReadGate, set_floor_filter, …}` | module moved down | `aft-db` | Share request-local read policy and remote mutation state without the index or composition. |
| `path_filters::{PathFilters, build_path_filters, build_globset, relative_to_root, to_glob_path}` | items moved down | `aft-db` | Confinement fence floor filter. |
| `data_layer::read_fence`, `context::confinement` and `search_index::{PathFilters, build_path_filters}` | compatibility re-exports | `aft-engine` | Preserve the request-scope API, including the typed floor filter. |
| `context::confinement::with_root` | compatibility adapter | `aft-engine` | Derive a read gate from the actual `AppContext` before calling the lower confinement scope. |
| `semantic_status::{SemanticIndexStatus, SemanticRefreshAccounting, SEMANTIC_MASS_REFRESH_MIN_FILES}` | items moved down | `aft-base` | Refresh accounting is plain per-root state and does not need an index worker. |
| `data_layer::status` and the corresponding `context` status names | compatibility re-exports | `aft-engine` | Keep old semantic-status paths. |
| `progress_sender::{ProgressSender, SharedProgressSender}` | items moved down | `aft-base` | Progress handles carry only protocol frames. |
| `context::{ProgressSender, SharedProgressSender}` | compatibility re-exports | `aft-engine` | Preserve the old sender paths. |
| `project_walk::apply_project_ignore_rules` and `secret::call_context` | existing lower owners | `aft-base` | Ignore policy and call-local secret scopes are already lower-owned. |
| `context::apply_project_ignore_rules` and `secret::call_context` | compatibility re-exports | `aft-engine` | Preserve the old paths. |
| `callgraph::walk_project_files` | existing lower owner | `aft-parse` | The format walker can use the lower callgraph API directly. |
| `pinned_executable::startup_image_metadata`, `secret::transport::ManagementSession` and `secret::eligibility::alfonso_of_kind` | existing lower owners | `aft-base` | Use the lower implementations directly at the crossings. |
| `context::App` | stays in composition | `aft-engine` | Owns live LSP, shell and actor handles; add the consumer port when separating `aft-shell`. |
| `watcher_backend::counters::WatcherCounters` | stays in composition | `aft-engine` | Owns live flush and shell-journal handles; add the consumer port when separating `aft-index`. |
| `root_cache::{WriterLease, ArtifactAccess}` | items moved down | `aft-db` | Share cache access with index and composition through one process lease registry. |
| `test_env`, `subc_fake` and shared `test_storage` primitives | existing test-support helpers | `aft-base` | Lower tests share hermetic environment, daemon fakes and test-storage checks. |
| `fake_lsp_test_helper` and its `binary_cache` module | test-support helper | `aft-engine` | Engine LSP tests own this helper; its feature-compiled inputs stay in the engine package. |
| `test_storage` context adapter | stays in composition | `aft-engine` | Test `AppContext` constructors use private temporary directories instead of the live database root. |
| `tests/fixtures/root_identity_upgrade/cases.json` | test fixture | `aft-db` | Keep the facade copy byte-identical until final composition cleanup can remove it after its last reader is gone. |

## Edit area

| Item | Kind | Owning crate | Reason |
|---|---|---|---|
| `agent_data`, `backup`, `checkpoint`, `edit`, `extract`, `file_read_exact`, `format`, `hashline`, `imports`, `indent`, `patch`, `symbol_diff` | modules moved down | `aft-edit` | Keep edit and recovery policy together below composition. |
| `agent_data`, `backup`, `checkpoint`, `edit`, `extract`, `file_read_exact`, `format`, `hashline`, `imports`, `indent`, `patch`, `symbol_diff` | compatibility re-exports | `aft-engine` | Keep the paths callers use before and after the split. |
| `edit::ports::PostWriteDiagnostics` | port | `aft-edit` | Render the inspect outcome's reply fields without a dependency on LSP. |
| `impl PostWriteDiagnostics for PostEditWaitOutcome` | adapter | `aft-engine` | Render the existing wire fields from the concrete outcome. |
| `AppContext::lsp_post_write` | compatibility adapter | `aft-engine` | Return an owned result port where edit replies consume it; LSP wait and alert paths still use the concrete outcome. |
| `edit::ports::BackupSnapshots` | port | `aft-edit` | Request snapshots and skip reasons without holding the store lock across the boundary. |
| `edit::ports::SnapshotRequest` | port request | `aft-edit` | Carry the original snapshot arguments and an optional captured file together. |
| `edit::ports::SnapshotResult` | port result | `aft-edit` | Report whether a snapshot was taken, independently of the observation check. |
| `impl BackupSnapshots for AppContext` | adapter | `aft-engine` | Recheck the admitted file under the store lock before creating its backup. |
| `edit::auto_backup` | compatibility wrapper | `aft-engine` | Adapt the existing context argument to the snapshot port. |
| `edit::auto_backup_observed` | compatibility wrapper | `aft-engine` | Adapt the existing context argument without changing edit admission. |
| `edit::auto_backup_from_capture` | compatibility wrapper | `aft-engine` | Adapt the existing context argument without taking a new capture. |
| `edit::attach_backup_skipped_reason` | compatibility wrapper | `aft-engine` | Resolve the same skip reason through the snapshot port. |
| `edit::tool_launcher::run_prepared` | visibility widened | `aft-edit` | The engine supplies the unchanged launch policy to the edit pipeline. |
| `edit::test_launcher::Unconfined` | test-only helper | `aft-edit` | Prepare the same unconfined launch for area tests without a shell dependency. |
| `edit::test_support::swift::run_swiftc_for_test` | test-support driver | `aft-edit` | Exercise the real launch policy against the Swift syntax checker in composition. |
| `format::ruff_probe_refused_for_test` | test-support driver | `aft-edit` | Observe the formatter's exact refusal classification in composition. |
| `edit::test_support::{bash, swift}` | test-support adapters | `aft-edit` | Keep Bash and Swift fixtures available through the engine test bridge. |
| `edit::test_support::{diff_budget_remaining, set_diff_budget_remaining, diff_budget_omitted, BoundedDiffText}` | test-support adapters | `aft-edit` | Retain the old engine test bridge without exposing edit counters in production. |
| `edit::file_reporter_registered` | test-support visibility widened | `aft-edit` | The engine bridge verifies that the file reporter is wired. |
| `hashline::integration::build_display_metadata` | test-support visibility widened | `aft-edit` | Retain the existing engine bridge's display renderer. |
| `backup::reset_capture_read_count` | test-support visibility widened | `aft-edit` | Retain the engine bridge's capture-read accounting. |
| `backup::capture_read_count` | test-support visibility widened | `aft-edit` | Retain the engine bridge's capture-read accounting. |
| `BackupStore::resident_entry_count` | test-support visibility widened | `aft-edit` | Retain the engine bridge's residency checks. |
| `BackupStore::fail_next_disk_write_for_tests` | test-support visibility widened | `aft-edit` | Retain the engine bridge's backup failure injection. |
| `BackupStore::release_after_next_snapshot_for_tests` | test-support visibility widened | `aft-edit` | Retain the engine bridge's snapshot release hook. |
| `BackupStore::set_discovery_entry_limit_for_tests` | test-support visibility widened | `aft-edit` | Retain the engine bridge's discovery bounds checks. |
| `BackupStore::expire_discovery_for_tests` | test-support visibility widened | `aft-edit` | Retain the engine bridge's discovery expiry checks. |
| `BackupStore::uses_db_pool` | test-support visibility widened | `aft-edit` | Assert the store shares the composed context's connection. |
| `BackupStore::disk_io_count_for_tests` | test-support visibility widened | `aft-edit` | Retain the engine bridge's disk I/O accounting. |
| `BackupStore::discard_paths_for_tests` | test-support adapter | `aft-edit` | Inspect cleanup through the engine bridge without making the field public. |
| `BackupStore::add_created_dir_for_test` | test-support visibility widened | `aft-edit` | Retain engine bridge fixtures for undo authorization. |
| `BackupStore::rekey_for_test` | test-support visibility widened | `aft-edit` | Retain engine bridge fixtures for undo authorization. |
| `BackupStore::history_meta_path_for_test` | test-support visibility widened | `aft-edit` | Retain engine bridge fixtures for durable backup metadata. |
| `BackupStore::history_content_reads_for_test` | test-support visibility widened | `aft-edit` | Retain the engine bridge's history content accounting. |
| `BackupStore::new_for_unit_test` | test-support visibility widened | `aft-edit` | Keep explicit temporary backup policy for engine unit tests. |
| `CheckpointStore::unbound_in_for_test` | test-support visibility widened | `aft-edit` | Retain engine bridge fixtures for explicit checkpoint stores. |
| `CheckpointStore::lock_path_for_test` | test-support visibility widened | `aft-edit` | Retain engine bridge fixtures for checkpoint lock contention. |
| `CheckpointStore::set_lock_path_for_test` | test-support visibility widened | `aft-edit` | Retain engine bridge fixtures for checkpoint lock contention. |
| `CheckpointStore::use_namespace_request` | visibility widened | `aft-edit` | Composition keeps the requested namespace when exchanging checkpoint stores. |
| `CheckpointStore::resident_session_count` | visibility widened | `aft-edit` | Status reconciles per-session residency with the durable count. |
| `CheckpointStore::status_count` | visibility widened | `aft-edit` | Status reads the same checkpoint count without taking a mutation lock. |
| `checkpoint::status_read_counters_for_test` | test-support visibility widened | `aft-edit` | Composed status tests track checkpoint directory reads and lock acquisitions. |
| `BackupStore::status_inventory` | visibility widened | `aft-edit` | Composition copies the status inputs before releasing the backup lock. |
| `backup::StatusInventory` | visibility widened | `aft-edit` | Carry the backup status inputs out of the lock without exposing store internals. |
| `StatusInventory::tracked_file_count` | visibility widened | `aft-edit` | Status counts tracked files from the detached inventory. |
| `BackupStore::status_discovery_entries_for_test` | test-support visibility widened | `aft-edit` | Composed status tests count directory traversal in the real backup store. |
| `backup::latency::Timer` | visibility widened | `aft-edit` | Keep the existing scoped backup lock timer in the composition adapter. |
| `backup::latency::time` | visibility widened | `aft-edit` | Record lock wait in the original edit phase accounting. |
| `ReadHooks::with_mid_read` | test-support visibility widened | `aft-edit` | Keep the exact-read hook available to engine tests. |
| `format::SubprocessOutcome` | visibility widened | `aft-edit` | Share the same command capture result with inspect, shell, and GitHub callers. |
| `SubprocessOutcome::stdout` | visibility widened | `aft-edit` | Those callers parse the captured output. |
| `SubprocessOutcome::stdout_utf8_valid` | visibility widened | `aft-edit` | Consumers reject malformed UTF-8 rather than reading replacement characters. |
| `SubprocessOutcome::stderr` | visibility widened | `aft-edit` | Consumers retain the existing error reporting. |
| `SubprocessOutcome::status` | visibility widened | `aft-edit` | Consumers retain exit status checks. |
| `SubprocessOutcome::truncated` | visibility widened | `aft-edit` | Consumers reject incomplete capture. |
| `format::AVAILABILITY_PROBE_BUDGET` | visibility widened | `aft-edit` | GitHub probes retain their existing deadline. |
| `format::capture_probe_command` | visibility widened | `aft-edit` | GitHub probes use the existing bounded capture and cancellation. |
| `format::capture_bounded_command` | visibility widened | `aft-edit` | Shell environment imports use the existing bounded capture. |
| `format::wait_with_deadline` | visibility widened | `aft-edit` | Inspect and shell wait under their existing deadlines. |
| `format::parse_cargo_messages` | visibility widened | `aft-edit` | Inspect retains the same Cargo diagnostics parser. |
| `format::isolate_in_process_group` | re-export visibility widened | `aft-edit` | Preserve the existing format path for lower process setup used by inspect, index, shell, and GitHub. |
| `format::kill_process_tree` | re-export visibility widened | `aft-edit` | Preserve the existing format path for lower process cleanup used by index and GitHub. |
| `format::first_batch_skip_reason_for_test` | test-support driver | `aft-edit` | Read the first batch result's skip reason under the real sandbox policy. |
| `backup::ORDINARY_SCHEMA_VERSION` | re-export visibility widened | `aft-edit` | Engine's compatibility test reads the same ordinary backup schema constant. |

## Index area

| Item | Kind | Owning crate | Reason |
|---|---|---|---|
| `data_layer`, `semantic_index`, `search_index`, `search_state`, `grep_executor`, `large_read`, `search_b2`, `synapse_embed`, `local_embed`, `feature_status`, `semantic_admission`, `watcher_filter`, `watcher_backend`, `watcher`, `git_index_reader`, `callgraph_store`, `calls`, `grammar`, `luau`, `url_fetch`, `ort_pin`, `memory_census` | modules moved down | `aft-index` | Keep index storage, query execution, embedding, grammar and event ingestion below composition. |
| Index module paths | compatibility re-exports | `aft-engine` | Keep the paths used by tools, daemon, and facade, except for the private Luau implementation. |
| `root_cache`, `cache_location`, `fs_lock`, `path_identity`, `production_storage` | compatibility re-exports | `aft-engine` | Keep cache access and its filesystem policy on a single lower-owned implementation. |
| `root_cache::WriterLease::acquire_shared_until` | visibility widened | `aft-db` | Keep the existing writer lease acquisition and retry budget in composed cache callers. |
| `root_cache::{canonical_root, check_cache_access, identity_storage_root, prepare_third_party_cache_dir}` | re-export visibility widened | `aft-db` | Use one cache identity and access policy across index and engine. |
| `root_cache::{prepare_cache_root_for_test, sweep_all_read_markers}` | test-support re-exports | `aft-db` | Make lower cache preparation and marker sweeping available to moved tests. |
| `IndexHost::explicit_task_artifacts` | port | `aft-index` | Grep asks for visible task artifacts without acquiring shell state or bypassing session and root checks. |
| `impl IndexHost for AppContext` (`explicit_task_artifacts`) | adapter | `aft-engine` | Delegate the artifact query to the real Bash registry. |
| `IndexHost::semantic_building` | port | `aft-index` | Give the backend probe a blocking observation without exposing the status lock. |
| `IndexHost::try_semantic_index_status` | port | `aft-index` | Observe status without waiting or giving the caller a lock guard. |
| `IndexHost::try_semantic_index_resident` | port | `aft-index` | Distinguish a busy index slot from an empty one without exposing its lock. |
| `IndexHost::semantic_build_in_progress` | port | `aft-index` | A present or busy build receiver remains evidence of an in-progress build. |
| `impl IndexHost for AppContext` (semantic observations) | adapter | `aft-engine` | Keep lock acquisition and poison handling in composition. |
| `CallgraphQueryHost` | port | `aft-index` | Supply only the observed runtime facts and accounting needed by call graph queries. |
| `impl CallgraphQueryHost for AppContext` | adapter | `aft-engine` | Read the real project's ignore and root state and record call-graph timings and outcomes. |
| `data_layer::grammar::prepare` | port consumer | `aft-index` | Prepare Luau against the same pinned configuration, only when the language matches. |
| `data_layer::todo_counts` | module exposure | `aft-index` | Expose plain TODO-count data to the inspect projection. |
| `TodoCounts` | item moved down | `aft-index` | Decode comment counts without a dependency on the inspect scanner. |
| `TodoCounts::total` | visibility widened | `aft-index` | Retain inspect's total marker count. |
| `TodoCounts::by_kind` | visibility widened | `aft-index` | Retain inspect's per-marker counts. |
| `TodoCounts::of` | visibility widened | `aft-index` | The inspect scanner and checkout use exactly the same counting rule. |
| `TodoSummary` | visibility widened | `aft-index` | Inspect classifies the checkout's count summary against its path. |
| `TodoSummary::language` | visibility widened | `aft-index` | Keep inspect's stored-language check. |
| `TodoSummary::counts` | visibility widened | `aft-index` | Keep inspect's stored marker counts. |
| `watcher_backend::counters` | visibility widened | `aft-index` | Composition uses the same per-root watcher counters as native backends. |
| `WatcherChangeSink` | port | `aft-index` | Deliver raw watcher events and losses without naming the Bash journal. |
| `WatcherChanges` | handle | `aft-index` | Hold the optional sink in one slot shared with the root's counters. |
| `WatcherCounters::bash_changes` | handle accessor | `aft-index` | Access the existing root-local sink slot without exposing its lock. |
| `WatcherChanges::{get, get_or_init}` | handle operations | `aft-index` | Observe or initialize the single sink without replacing it. |
| `impl WatcherChangeSink for Journal` | adapter | `aft-engine` | Keep the existing Bash event admission and journal gap logic. |
| `WatcherCountersHandle` | compatibility wrapper | `aft-engine` | Preserve the existing watcher-counter, flush and journal call paths. |
| `BashChanges` | compatibility wrapper | `aft-engine` | Type the sink as the same Bash journal used by shell capture. |
| `BashChanges::get_or_init` | compatibility wrapper | `aft-engine` | Leave repeated initialization idempotent for inspect and shell callers. |

| `watcher_filter::config_events::is_config_file_name` | visibility widened | `aft-index` | Use the watcher’s filename rule when installing configuration watches. |
| `data_layer::status::view_load_cause` | visibility widened | `aft-index` | Classify view load failures consistently in status and parent search. |
| `data_layer::status::VIEW_LOAD_DEGRADED_REASON` | visibility widened | `aft-index` | Keep the status reason used for degraded view loads. |
| `data_layer::status::VIEW_LOAD_LOG_INTERVAL` | visibility widened | `aft-index` | Keep the retry log throttle in composition and parent search. |
| `data_layer::ignore_rules` | visibility widened | `aft-index` | Let composition update the checkout’s ignore rules. |
| `data_layer::dead_code_reads` | visibility widened | `aft-index` | Let inspect read source for the dead-code projection. |
| `data_layer::bound_checkouts` | visibility widened | `aft-index` | Let inspect look up the root’s already-bound checkout. |
| `data_layer::dead_code_snapshot::CALLGRAPH_PROVENANCE_REEXPORT` | visibility widened | `aft-index` | Use the same re-export labels in the dead-code scan. |
| `data_layer::dead_code_snapshot::CALLGRAPH_PROVENANCE_TREESITTER` | visibility widened | `aft-index` | Use the same parser provenance in the dead-code scan. |
| `data_layer::dead_code_snapshot::DISPATCHED_CALLEE_SEPARATOR` | visibility widened | `aft-index` | Interpret dispatched callee records with the same separator. |
| `data_layer::dead_code_snapshot::DEFAULT_EXPORT_MARKER_KIND` | visibility widened | `aft-index` | Recognize the stored default-export marker in inspect. |
| `data_layer::dead_code_graph::CallGraphFiles` | visibility widened | `aft-index` | Let inspect provide the files used by graph section analysis. |
| `data_layer::dead_code_graph::DeadCodeGraph` | visibility widened | `aft-index` | Use the existing graph model in inspect’s dead-code analysis. |
| `data_layer::dead_code_graph::SeedClass` | visibility widened | `aft-index` | Keep inspect’s identification of graph entry points. |
| `data_layer::checkout::GraphIdentity` | visibility widened | `aft-index` | Let inspect compare live and published graph identity. |
| `data_layer::checkout::PathTodos` | visibility widened | `aft-index` | Use the existing stored TODO-marker states in inspect. |
| `data_layer::checkout::PathTodoCounts` | visibility widened | `aft-index` | Use the existing stored TODO-count states in inspect. |
| `data_layer::checkout::ViewGraphFiles` | visibility widened | `aft-index` | Let inspect traverse the same checkout snapshot. |
| `watcher_filter::watcher_path_is_infra_skip_under` | visibility widened | `aft-index` | Apply the watcher’s infrastructure exclusion while draining events. |
| `watcher_filter::watcher_path_is_ignored_by_matcher_with_kind` | visibility widened | `aft-index` | Use the watcher’s path filter when recording changes during a Bash command. |
| `watcher_filter::queued_path_is_ignored_by_matcher` | visibility widened | `aft-index` | Apply the same ignore rules to queued events in composition. |
| `watcher_filter::WatcherWalkBoundary` | visibility widened | `aft-index` | Apply the watcher’s nested-root boundary when walking parent scopes. |
| `watcher_filter::persist_watcher_observations` | visibility widened | `aft-index` | Persist observations when composition drains the watcher. |
| `data_layer::surfaces::memory_estimate` | visibility widened | `aft-index` | Report index memory on the root’s memory surface. |
| `data_layer::surfaces::DataLayerMemory` | visibility widened | `aft-index` | Carry computed index memory to the root status report. |
| `data_layer::surfaces::memory_estimate_for_rollup` | visibility widened | `aft-index` | Use the index’s rollup without repeating per-root accounting. |
| `data_layer::surfaces::storage_refusal` | visibility widened | `aft-index` | Report the same storage refusal in index and push status. |
| `data_layer::surfaces::refusal_reason` | visibility widened | `aft-index` | Share the stored-format refusal wording across status surfaces. |
| `data_layer::surfaces::refusal_json` | visibility widened | `aft-index` | Share the structured stored-format refusal across status surfaces. |
| `data_layer::surfaces::report_for_push` | visibility widened | `aft-index` | Build push status from the same index report. |
| `data_layer::surfaces::apply_to_status_operator` | visibility widened | `aft-index` | Preserve the operator-specific status projection. |
| `data_layer::surfaces::apply_to_status_push` | visibility widened | `aft-index` | Preserve the push-specific status projection. |
| `data_layer::surfaces::apply_to_status` | visibility widened | `aft-index` | Preserve the public status projection. |
| `data_layer::runtime::memory_profile_changed` | visibility widened | `aft-index` | Refresh index admission after the engine publishes a memory profile. |
| `data_layer::runtime::has_family_store` | visibility widened | `aft-index` | Check for an existing external parent store without binding it. |
| `data_layer::runtime::bind_foreground` | visibility widened | `aft-index` | Bind the external parent store through the existing index path. |
| `data_layer::parent_session::publish` | visibility widened | `aft-index` | Publish the composed parent view to the index session registry. |
| `data_layer::parent_session::withdraw` | visibility widened | `aft-index` | Remove the same parent view when its composed session ends. |
| `data_layer::parent_query::grep_fan_out` | visibility widened | `aft-index` | Use the index’s parent grep query from composition. |
| `data_layer::parent_query::glob_fan_out` | visibility widened | `aft-index` | Use the index’s parent glob query from composition. |
| `semantic_index::files_with_changed_content` | visibility widened | `aft-index` | Filter semantic refresh to files with changed content. |

| `data_layer::dead_code_snapshot::EntryPoints` | visibility widened | `aft-index` | Let inspect supply its language entry-point policy. |
| `data_layer::dead_code_reads::Lookup` | visibility widened | `aft-index` | Keep inspect’s existing keyed reads and their invalidation tracking. |
| `data_layer::dead_code_reads::record` | visibility widened | `aft-index` | Keep inspect’s existing keyed reads and their invalidation tracking. |
| `data_layer::dead_code_reads::Recording` | visibility widened | `aft-index` | Keep inspect’s existing keyed reads and their invalidation tracking. |
| `data_layer::dead_code_reads::Recording::start` | visibility widened | `aft-index` | Keep inspect’s existing keyed reads and their invalidation tracking. |
| `data_layer::dead_code_reads::Recording::finish` | visibility widened | `aft-index` | Keep inspect’s existing keyed reads and their invalidation tracking. |
| `data_layer::dead_code_reads::Tracked` | visibility widened | `aft-index` | Keep inspect’s existing keyed reads and their invalidation tracking. |
| `data_layer::dead_code_reads::Tracked::new` | visibility widened | `aft-index` | Keep inspect’s existing keyed reads and their invalidation tracking. |
| `data_layer::dead_code_reads::Tracked::all` | visibility widened | `aft-index` | Keep inspect’s existing keyed reads and their invalidation tracking. |
| `data_layer::dead_code_reads::Tracked::record_key` | visibility widened | `aft-index` | Keep inspect’s existing keyed reads and their invalidation tracking. |
| `data_layer::dead_code_reads::Tracked::record_as` | visibility widened | `aft-index` | Keep inspect’s existing keyed reads and their invalidation tracking. |
| `data_layer::dead_code_reads::Tracked::get` | visibility widened | `aft-index` | Keep inspect’s existing keyed reads and their invalidation tracking. |
| `data_layer::dead_code_reads::Tracked::contains (map)` | visibility widened | `aft-index` | Keep inspect’s existing keyed reads and their invalidation tracking. |
| `data_layer::dead_code_reads::Tracked::contains (set)` | visibility widened | `aft-index` | Keep inspect’s existing keyed reads and their invalidation tracking. |
| `data_layer::dead_code_reads::pair_key` | visibility widened | `aft-index` | Keep inspect’s existing keyed reads and their invalidation tracking. |
| `data_layer::dead_code_reads::path_key` | visibility widened | `aft-index` | Keep inspect’s existing keyed reads and their invalidation tracking. |
| `data_layer::dead_code_reads::digest` | visibility widened | `aft-index` | Keep inspect’s existing keyed reads and their invalidation tracking. |
| `data_layer::ignore_rules::IgnoreInputSnapshot` | visibility widened | `aft-index` | Use the watcher’s ignore-input comparison when composition reloads rules. |
| `data_layer::ignore_rules::IgnoreRuleChange` | visibility widened | `aft-index` | Use the watcher’s ignore-input comparison when composition reloads rules. |
| `data_layer::ignore_rules::IgnoreInputSnapshot::collect` | visibility widened | `aft-index` | Use the watcher’s ignore-input comparison when composition reloads rules. |
| `data_layer::ignore_rules::IgnoreInputSnapshot::changed_paths_match_bytes` | visibility widened | `aft-index` | Use the watcher’s ignore-input comparison when composition reloads rules. |
| `data_layer::ignore_rules::IgnoreInputSnapshot::has_same_effective_inputs` | visibility widened | `aft-index` | Use the watcher’s ignore-input comparison when composition reloads rules. |
| `data_layer::ignore_rules::IgnoreInputSnapshot::classify_against` | visibility widened | `aft-index` | Use the watcher’s ignore-input comparison when composition reloads rules. |

| `bound_checkouts::RootKind` | visibility widened | `aft-index` | Let inspect distinguish one checkout, a folder of repositories, and a withheld root. |
| `bound_checkouts::root_kind` | visibility widened | `aft-index` | Read the root classification recorded by configure. |
| `bound_checkouts::note_root` | visibility widened | `aft-index` | Configure records the root classification in the index’s registry. |
| `bound_checkouts::newest_bound_checkouts` | boundary accessor | `aft-index` | Return live checkouts in inventory order without exposing the registry lock. |
| `bound_checkouts::bound_checkout_roots` | boundary accessor | `aft-index` | Return the resolved roots and checkouts for inspect’s TODO scan. |

| `data_layer::checkout::{PathTodoCounts, PathTodos, TodoSummary}` | compatibility re-exports | `aft-index` | Keep the checkout paths for the TODO projection types in their sibling module. |
| `data_layer::runtime::root` | visibility widened | `aft-index` | Keep composition’s checkout and maintenance lifecycle unchanged. |
| `data_layer::runtime::open_planes` | visibility widened | `aft-index` | Keep composition’s checkout and maintenance lifecycle unchanged. |
| `data_layer::runtime::set_background_refresh` | visibility widened | `aft-index` | Keep composition’s checkout and maintenance lifecycle unchanged. |
| `data_layer::runtime::on_status_change` | visibility widened | `aft-index` | Keep composition’s checkout and maintenance lifecycle unchanged. |
| `data_layer::runtime::take_todos_readable` | visibility widened | `aft-index` | Keep composition’s checkout and maintenance lifecycle unchanged. |
| `data_layer::runtime::note_paths` | visibility widened | `aft-index` | Keep composition’s checkout and maintenance lifecycle unchanged. |
| `data_layer::runtime::rescan` | visibility widened | `aft-index` | Keep composition’s checkout and maintenance lifecycle unchanged. |
| `data_layer::runtime::set_executor` | visibility widened | `aft-index` | Keep composition’s checkout and maintenance lifecycle unchanged. |
| `data_layer::runtime::read_meaning_only` | visibility widened | `aft-index` | Keep composition’s checkout and maintenance lifecycle unchanged. |
| `data_layer::surfaces::health` | visibility widened | `aft-index` | Keep projection and accounting on the existing index report. |
| `data_layer::surfaces::fold_into` | visibility widened | `aft-index` | Keep projection and accounting on the existing index report. |
| `data_layer::ignore_rules::build_matcher` | visibility widened | `aft-index` | Build the same matcher when composition reloads ignore rules. |
| `watcher_backend::counters::query_delivery_pending` | visibility widened | `aft-index` | Keep composition’s watcher drain on the existing event and rescan state. |
| `watcher_backend::counters::unflushed_paths` | visibility widened | `aft-index` | Keep composition’s watcher drain on the existing event and rescan state. |
| `watcher_backend::counters::start_rescan` | visibility widened | `aft-index` | Keep composition’s watcher drain on the existing event and rescan state. |
| `watcher_backend::counters::take_overflow_ignore_relevance` | visibility widened | `aft-index` | Keep composition’s watcher drain on the existing event and rescan state. |
| `watcher_backend::counters::begin_rescan` | visibility widened | `aft-index` | Keep composition’s watcher drain on the existing event and rescan state. |
| `watcher_backend::counters::finish_rescan` | visibility widened | `aft-index` | Keep composition’s watcher drain on the existing event and rescan state. |
| `watcher_backend::counters::finish_rescan_walk` | visibility widened | `aft-index` | Keep composition’s watcher drain on the existing event and rescan state. |
| `data_layer::parent::keep_only` | visibility widened | `aft-index` | Keep composed parent sessions on the same index lifecycle. |
| `data_layer::parent::note_paths` | visibility widened | `aft-index` | Keep composed parent sessions on the same index lifecycle. |
| `data_layer::parent::rescan` | visibility widened | `aft-index` | Keep composed parent sessions on the same index lifecycle. |
| `data_layer::parent::set_executor` | visibility widened | `aft-index` | Keep composed parent sessions on the same index lifecycle. |
| `search_state::rerank::slot::reconcile` | visibility widened | `aft-index` | Keep composition on the existing search state and memory observations. |
| `search_state::memo::try_clear_idle` | visibility widened | `aft-index` | Keep composition on the existing search state and memory observations. |
| `semantic_index::memory_shape` | visibility widened | `aft-index` | Keep composition on the existing search state and memory observations. |
| `data_layer::dead_code_graph::files` | visibility widened | `aft-index` | Keep inspect’s existing access to the graph data it analyzes. |
| `data_layer::dead_code_graph::open` | visibility widened | `aft-index` | Keep inspect’s existing access to the graph data it analyzes. |
| `data_layer::dead_code_graph::root` | visibility widened | `aft-index` | Keep inspect’s existing access to the graph data it analyzes. |
| `data_layer::dead_code_graph::file_paths` | visibility widened | `aft-index` | Keep inspect’s existing access to the graph data it analyzes. |
| `data_layer::dead_code_graph::resolve` | visibility widened | `aft-index` | Keep inspect’s existing access to the graph data it analyzes. |
| `data_layer::dead_code_graph::resolves_lexically` | visibility widened | `aft-index` | Keep inspect’s existing access to the graph data it analyzes. |
| `data_layer::dead_code_graph::unknown_live` | visibility widened | `aft-index` | Keep inspect’s existing access to the graph data it analyzes. |
| `data_layer::dead_code_graph::project_file` | visibility widened | `aft-index` | Keep inspect’s existing access to the graph data it analyzes. |
| `data_layer::dead_code_graph::own_entry_point_symbols` | visibility widened | `aft-index` | Keep inspect’s existing access to the graph data it analyzes. |
| `data_layer::dead_code_graph::open_lazy` | visibility widened | `aft-index` | Keep inspect’s existing access to the graph data it analyzes. |
| `data_layer::dead_code_graph::section_dir` | visibility widened | `aft-index` | Keep inspect’s existing access to the graph data it analyzes. |
| `data_layer::dead_code_graph::all_exported_symbols` | visibility widened | `aft-index` | Keep inspect’s existing access to the graph data it analyzes. |
| `data_layer::dead_code_graph::exported_symbols` | visibility widened | `aft-index` | Keep inspect’s existing access to the graph data it analyzes. |
| `data_layer::dead_code_graph::compact_targets` | visibility widened | `aft-index` | Keep inspect’s existing access to the graph data it analyzes. |
| `data_layer::dead_code_graph::failure` | visibility widened | `aft-index` | Keep inspect’s existing access to the graph data it analyzes. |
| `data_layer::dead_code_graph::go_packages` | visibility widened | `aft-index` | Keep inspect’s existing access to the graph data it analyzes. |
| `data_layer::dead_code_graph::patch_go_packages` | visibility widened | `aft-index` | Keep inspect’s existing access to the graph data it analyzes. |
| `data_layer::dead_code_graph::exported_symbols_of` | visibility widened | `aft-index` | Keep inspect’s existing access to the graph data it analyzes. |
| `data_layer::checkout::identity` | visibility widened | `aft-index` | Read the same checkout snapshot and stored records from inspect. |
| `data_layer::checkout::published_identity` | visibility widened | `aft-index` | Read the same checkout snapshot and stored records from inspect. |
| `data_layer::checkout::release_dead_code_snapshot` | visibility widened | `aft-index` | Read the same checkout snapshot and stored records from inspect. |
| `data_layer::checkout::dead_code_snapshot_in` | visibility widened | `aft-index` | Read the same checkout snapshot and stored records from inspect. |
| `data_layer::checkout::todo_markers` | visibility widened | `aft-index` | Read the same checkout snapshot and stored records from inspect. |
| `data_layer::checkout::todo_counts` | visibility widened | `aft-index` | Read the same checkout snapshot and stored records from inspect. |
| `data_layer::checkout::rows_dead_code_snapshot` | visibility widened | `aft-index` | Read the same checkout snapshot and stored records from inspect. |
| `data_layer::checkout::dead_code_files` | visibility widened | `aft-index` | Read the same checkout snapshot and stored records from inspect. |
| `watcher_filter::for_root` | visibility widened | `aft-index` | Keep parent walks within the watcher’s directory boundary. |
| `watcher_filter::allows_directory` | visibility widened | `aft-index` | Keep parent walks within the watcher’s directory boundary. |
| `data_layer::parent_session::new` | visibility widened | `aft-index` | Keep composed parent sessions on the same index lifecycle. |

| `inspect::scanners::dead_code::index::TrackedIndex` | private extension trait | `aft-engine` | Keep keyed reads with the inspect-owned index type while recording them in the shared tracker. |
| `inspect::scanners::dead_code::index::TrackedPairIndex` | private extension trait | `aft-engine` | Keep inspect’s tracked lookup of a file and symbol together. |
| `inspect::scanners::dead_code::TrackedRustItems` | private extension trait | `aft-engine` | Keep inspect’s tracked lookup of a named Rust item. |
| `WatcherCounters::take_checkout_watcher_gap` | boundary accessor | `aft-index` | Consume the pending gap without exposing the atomic flag. |
| `EmbeddingBackendHealthSnapshot::next_retry_ms` | visibility widened | `aft-index` | Keep the backend retry time in composed status. |
| `DataStoreReport::callgraph` | visibility widened | `aft-index` | Compose call-graph status from the existing plane observation. |
| `DataStoreReport::meaning` | visibility widened | `aft-index` | Compose semantic health from the existing meaning plane. |
| `DataStoreReport::refusal` | visibility widened | `aft-index` | Keep reporting the exact stored-format refusal. |
| `GraphIdentity::record` | visibility widened | `aft-index` | Compare the stored record identity used for dead-code reuse. |
| `GraphIdentity::fragment` | visibility widened | `aft-index` | Compare the stored fragment identity used for dead-code reuse. |
| `GraphIdentity::digest` | visibility widened | `aft-index` | Compare the stored graph digest used for dead-code reuse. |

| `FileProjection` | visibility widened | `aft-index` | Give the dead-code scanner the existing per-file graph facts. |
| `FileProjection::outbound_calls` | visibility widened | `aft-index` | Retain the call records used by dead-code analysis. |
| `FileProjection::unknown_live` | visibility widened | `aft-index` | Retain the unresolved-target seeds used by dead-code analysis. |
| `FileProjection::go_initializers` | visibility widened | `aft-index` | Retain the Go initializer groups used by dead-code analysis. |
| `MeaningReport` | visibility widened | `aft-index` | Let composition read health from the emitted meaning plane. |
| `MeaningReport::health` | visibility widened | `aft-index` | Keep the same semantic health projection in composition. |
| `WatcherRescanInterval::raw_events` | visibility widened | `aft-index` | Report the raw event count from the completed rescan interval. |

| `test_support::memory_walk::with_memory_walk_clock_for_test` | moved test utility | `aft-base` | Engine, index and upper tests use one thread-local memory-walk clock override. |
| `test_support::memory_walk::memory_walk_now_for_test` | moved test utility | `aft-base` | Read the shared override during synchronous memory walks, only with test support. |
| `context::{memory_walk_now_for_test, with_memory_walk_clock_for_test}` and `test_bridge::display_data::memory::with_memory_walk_clock_for_test` | test-support compatibility re-exports | `aft-engine` | Keep memory timing tests on the same clock through their old bridge path. |
| `ExactnessDeadCode` | test-hook port | `aft-index` | Run the graph exactness check with inspect’s real dead-code projection. |
| `data_layer::checkout::exactness::run` | test-hook compatibility wrapper | `aft-engine` | Supply inspect’s real entry-point and finding policies under the established example path. |
| `impl ExactnessDeadCode for InspectEntryPoints` | test-hook adapter | `aft-engine` | Preserve inspect findings alongside index rows in the exactness output. |
| `data_layer::surfaces::fixtures` | test-support visibility widened | `aft-index` | Share the same fixture checkout and status polling with composition tests. |
| `index_test_binding::{bind_for_test, serve_text_index_for_test, serve_text_only_for_test}` | test-support compatibility wrappers | `aft-engine` | Configure the actual context before waiting for the index store. |
| `data_layer::runtime::publication_fixture` | test-support visibility widened | `aft-index` | Exercise the same configurable publication fixture in composition tests. |
| `PublicationFixture::checkout_handle` | test-support accessor | `aft-index` | Bind the fixture’s real checkout to the composed test context. |
| `PublicationFixture::storage` | test-support accessor | `aft-index` | Point the composed test context at the fixture’s temporary storage. |
| `test_bridge::display_data::PublicationFixture` | test-support compatibility wrapper | `aft-engine` | Keep the context-building method at the existing bridge path. |
| `WatcherFilterConfig::note_raw_event_for_test` | test-support driver | `aft-index` | Observe a raw event in the same counters shared by filter and context. |
| `watcher_filter::test_helpers::verify_full_dispatch_backlog` | test-support driver | `aft-index` | Assert private backlog behavior with the actual application pending-state probe. |
| `local_embed::test_helpers::{native_test, wait_for_sessions}` | test-support helpers | `aft-index` | Keep one serial native-embedding guard and the same session-release check in both test binaries. |

| `data_layer::runtime::composed_checks::PublicationRouting` | test-support port | `aft-index` | Feed actual context route changes to the publication checks. |
| `data_layer::runtime::composed_checks` | test-support drivers | `aft-index` | Keep private queue, timer and publication assertions beside their implementation. |
| `index_publication_fixture::compose_context` | test-support adapter | `aft-engine` | Pin the actual index fixture in the running engine context. |
| `data_layer::meaning` | visibility widened | `aft-index` | Keep the preexisting embedding test fixtures available through the engine bridge. |
| `data_layer::meaning::use_constant_vectors` | test-support visibility widened | `aft-index` | Preserve the deterministic embedding fixture for parity checks. |

## Visibility widened for cross-crate callers

| Item | Kind | Owning crate | Reason |
|---|---|---|---|
| `process::test_hooks::with_signal_events` | test-support visibility widened | `aft-base` | Keep process and fault-injection callers on the same implementation. |
| `process::test_hooks::hide_members` | test-support visibility widened | `aft-base` | Keep process and fault-injection callers on the same implementation. |
| `process::test_hooks::make_members_unreadable` | test-support visibility widened | `aft-base` | Keep process and fault-injection callers on the same implementation. |
| `process::test_hooks::make_outsiders_unreadable` | test-support visibility widened | `aft-base` | Keep process and fault-injection callers on the same implementation. |
| `process::test_hooks::fail_listing` | test-support visibility widened | `aft-base` | Keep process and fault-injection callers on the same implementation. |
| `process::test_hooks::deny_sigkill` | test-support visibility widened | `aft-base` | Keep process and fault-injection callers on the same implementation. |
| `process::child_is_zombie_for_test` | test-support visibility widened | `aft-base` | Keep process and fault-injection callers on the same implementation. |
| `process::spawn_unreaped_zombie_for_test` | test-support visibility widened | `aft-base` | Keep process and fault-injection callers on the same implementation. |
| `process::terminate_pgid_with` | visibility widened | `aft-base` | Keep process and fault-injection callers on the same implementation. |
| `process::terminate_pgid_leader_grace` | visibility widened | `aft-base` | Keep process and fault-injection callers on the same implementation. |
| `process::recorded_process_liveness` | visibility widened | `aft-base` | Keep process and fault-injection callers on the same implementation. |
| `read_fence::Fence::without_floor_disclosure` | visibility widened | `aft-db` | Engine callers use the same request-local confinement state. |
| `read_fence::Fence::for_floor_filter_root` | visibility widened | `aft-db` | Engine callers use the same request-local confinement state. |
| `read_fence::Fence::record_floor` | visibility widened | `aft-db` | Engine callers use the same request-local confinement state. |
| `read_fence::Fence::record_source_exclusion` | visibility widened | `aft-db` | Engine callers use the same request-local confinement state. |
| `read_fence::Fence::record_matched_source_exclusion` | visibility widened | `aft-db` | Engine callers use the same request-local confinement state. |
| `read_fence::Fence::admits_walk_entry` | visibility widened | `aft-db` | Engine callers use the same request-local confinement state. |
| `read_fence::ReadGate` | visibility widened | `aft-db` | Engine callers use the same request-local confinement state. |
| `read_fence::ReadGate::new` | visibility widened | `aft-db` | Engine callers use the same request-local confinement state. |
| `read_fence::with_gate` | visibility widened | `aft-db` | Engine callers use the same request-local confinement state. |
| `read_fence::note_ask` | test-support visibility widened | `aft-db` | Engine callers use the same request-local confinement state. |
| `read_fence::ask_calls` | test-support visibility widened | `aft-db` | Engine callers use the same request-local confinement state. |
| `read_fence::admits_path_metadata` | visibility widened | `aft-db` | Engine callers use the same request-local confinement state. |
| `read_fence::for_request` | visibility widened | `aft-db` | Engine callers use the same request-local confinement state. |
| `read_fence::inherit` | visibility widened | `aft-db` | Engine callers use the same request-local confinement state. |
| `read_fence::denied_count` | visibility widened | `aft-db` | Engine callers use the same request-local confinement state. |
| `read_fence::with_root` | visibility widened | `aft-db` | Engine callers use the same request-local confinement state. |
| `read_fence::ConfinedIgnoreRules::is_ignored_below` | visibility widened | `aft-db` | Engine callers use the same request-local confinement state. |
| `build_breaker::BuildDeathBreaker::reset_open_calls_for_test` | test-support visibility widened | `aft-db` | Engine test drivers observe the real breaker counters. |
| `build_breaker::BuildDeathBreaker::open_calls_for_test` | test-support visibility widened | `aft-db` | Engine test drivers observe the real breaker counters. |
| `reader_floor::refresh_refused` | visibility widened | `aft-db` | Reuse floor bookkeeping and cache preparation across the crate boundary. |
| `reader_floor::prepare_artifact_root` | visibility widened | `aft-db` | Reuse floor bookkeeping and cache preparation across the crate boundary. |
| `reader_floor::prepare_cache_root_for_test` | test-support visibility widened | `aft-db` | Reuse floor bookkeeping and cache preparation across the crate boundary. |
| `reader_floor::clean_old_for_test` | test-support visibility widened | `aft-db` | Reuse floor bookkeeping and cache preparation across the crate boundary. |
| `persisted_format::ORDINARY_BACKUP_SCHEMA_VERSION` | visibility widened | `aft-db` | Engine format reporting and backups use the lower registry. |
| `persisted_format::has_floor_refusal` | visibility widened | `aft-db` | Engine format reporting and backups use the lower registry. |
| `persisted_format::replace_floor_refusals` | visibility widened | `aft-db` | Engine format reporting and backups use the lower registry. |
| `persisted_format::refusals_for_push` | visibility widened | `aft-db` | Engine format reporting and backups use the lower registry. |
| `root_identity::Identity` | visibility widened | `aft-db` | Engine root-cache and retry callers use the actual identity state. |
| `root_identity::artifact_access` | visibility widened | `aft-db` | Engine root-cache and retry callers use the actual identity state. |
| `root_identity::ConfiguredArtifactAccess` | visibility widened | `aft-db` | Engine root-cache and retry callers use the actual identity state. |
| `root_identity::ConfiguredArtifactAccess::project_root` | visibility widened | `aft-db` | Engine root-cache and retry callers use the actual identity state. |
| `root_identity::ConfiguredArtifactAccess::shared_key` | visibility widened | `aft-db` | Engine root-cache and retry callers use the actual identity state. |
| `root_identity::ConfiguredArtifactAccess::private_key` | visibility widened | `aft-db` | Engine root-cache and retry callers use the actual identity state. |
| `root_identity::ConfiguredArtifactAccess::borrow_only_shared` | visibility widened | `aft-db` | Engine root-cache and retry callers use the actual identity state. |
| `root_identity::ConfiguredArtifactAccess::registered` | visibility widened | `aft-db` | Engine root-cache and retry callers use the actual identity state. |
| `root_identity::configured_artifact_key` | visibility widened | `aft-db` | Engine root-cache and retry callers use the actual identity state. |
| `root_identity::RecordStamp` | visibility widened | `aft-db` | Engine root-cache and retry callers use the actual identity state. |
| `root_identity::record_stamp` | visibility widened | `aft-db` | Engine root-cache and retry callers use the actual identity state. |
| `root_identity::read` | visibility widened | `aft-db` | Engine root-cache and retry callers use the actual identity state. |
| `root_identity::legacy_pins_readable` | visibility widened | `aft-db` | Engine root-cache and retry callers use the actual identity state. |
| `root_identity::nearest_git_marker` | visibility widened | `aft-db` | Engine root-cache and retry callers use the actual identity state. |
| `root_identity::key::artifact_cache_key_for_build` | visibility widened | `aft-db` | Engine key lookups and test drivers share the actual identity state. |
| `root_identity::key::artifact_cache_key_read_only` | visibility widened | `aft-db` | Engine key lookups and test drivers share the actual identity state. |
| `root_identity::key::ArtifactCacheKeyTimings::root_walks` | visibility widened | `aft-db` | Engine key lookups and test drivers share the actual identity state. |
| `root_identity::key::artifact_cache_key_for_bind_profiled` | visibility widened | `aft-db` | Engine key lookups and test drivers share the actual identity state. |
| `root_identity::key::PROVISIONAL_IDENTITY_EXHAUSTED_REASON` | visibility widened | `aft-db` | Engine key lookups and test drivers share the actual identity state. |
| `root_identity::key::GIT_IDENTITY_UNAVAILABLE_REASON` | visibility widened | `aft-db` | Engine key lookups and test drivers share the actual identity state. |
| `root_identity::key::artifact_key_from_git_identity` | visibility widened | `aft-db` | Engine key lookups and test drivers share the actual identity state. |
| `root_identity::key::artifact_path_identity_key` | visibility widened | `aft-db` | Engine key lookups and test drivers share the actual identity state. |
| `root_identity::key::artifact_path_identity_key_for_test` | test-support visibility widened | `aft-db` | Engine key lookups and test drivers share the actual identity state. |
| `root_identity::key::canonical_git_root_commit_identity` | visibility widened | `aft-db` | Engine key lookups and test drivers share the actual identity state. |
| `root_identity::key::root_commit_identity_or_folder` | visibility widened | `aft-db` | Engine key lookups and test drivers share the actual identity state. |
| `root_identity::key::root_commit_git_runs_for_test` | test-support visibility widened | `aft-db` | Engine key lookups and test drivers share the actual identity state. |
| `root_identity::key::git_root_commit_probe_override_lock_for_test` | test-support visibility widened | `aft-db` | Engine key lookups and test drivers share the actual identity state. |
| `root_identity::key::GitRootCommitProbeOverrideGuard` | test-support visibility widened | `aft-db` | Engine key lookups and test drivers share the actual identity state. |
| `root_identity::key::force_git_root_commit_probe_transient_for_paths_for_test` | test-support visibility widened | `aft-db` | Engine key lookups and test drivers share the actual identity state. |
| `root_identity::key::force_git_root_commit_probe_held_transient_for_paths_for_test` | test-support visibility widened | `aft-db` | Engine key lookups and test drivers share the actual identity state. |
| `root_identity::key::force_git_root_commit_probe_commits_for_test` | test-support visibility widened | `aft-db` | Engine key lookups and test drivers share the actual identity state. |
| `root_identity::key::canonicalize_or_normalize` | visibility widened | `aft-db` | Engine key lookups and test drivers share the actual identity state. |
| `standing_roots::reconciliation_work_for_test` | test-support visibility widened | `aft-db` | Engine test drivers read lower reconciliation counters. |
| `process::test_hooks` | test-support visibility widened | `aft-base` | Engine unit tests inject failures into the lower implementation. |
| `process::start_new_session` | visibility widened | `aft-base` | Shell child setup and lower process fixtures share one session-start primitive. |
| `memory::profile::admitted` | visibility widened | `aft-base` | Engine builds pin the lower admission profile. |
| `path_filters::to_glob_path` | visibility widened | `aft-db` | The engine search code retains the identical path operation. |
| `root_identity::key::git_command_for_test` | test-support visibility widened | `aft-db` | The engine and lower identity tests share hermetic Git setup. |
| `fake_lsp_test_helper` | test-support visibility widened | `aft-engine` | Expose the helper to opt-in test consumers, not production crate builds. |
