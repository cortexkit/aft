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
| `standing_publication_lease::StandingPublicationLease` | port | `aft-db` | Standing publication verifies the engine's writer lease under the publish epoch. |
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
| `root_cache::{WriterLease, ArtifactAccess}` | stays in composition | `aft-engine` | Owns acquisition, filesystem policy and the process lease registry. |
| `test_env`, `subc_fake` and shared `test_storage` primitives | existing test-support helpers | `aft-base` | Lower tests share hermetic environment, daemon fakes and test-storage checks. |
| `fake_lsp_test_helper` and `test_storage` context adapter | stays in composition | `aft-engine` | No moved lower test needs the fake language server or builds an `AppContext`. |
| `tests/fixtures/root_identity_upgrade/cases.json` | test fixture | `aft-db` | Keep the facade copy byte-identical until final composition cleanup can remove it after its last reader is gone. |

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
