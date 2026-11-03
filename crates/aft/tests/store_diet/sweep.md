# String sweep references and index obligations

This is an index **plan**, not a claim that the current text schema can sweep
integer strings. Every column marked `interned string id` in
`table_encoding.json.gz` is a reference the sweep must check before deleting an
interned string: the string stays live while any such column holds its id.
The list below expands that set and
adds the identity-input columns a finished baseline store does not yet have.
An index named `diet_sweep_*` is a proposed additional index; it must be included
in the size preflight before the related database-format change is implemented. Existing secondary indexes
are listed at `callgraph_store/mod.rs:14157-14220`.

| Table | Referencing columns → index whose leading keys support equality SEARCH |
| --- | --- |
| files | path → primary key |
| nodes | file_path → idx_nodes_file; name → idx_nodes_name; scoped_name → idx_nodes_scoped; signature → **diet_sweep_nodes_signature** |
| refs | caller_file → idx_refs_caller_file; target_file → idx_refs_target_file; short_name → idx_refs_short_name; callee_last_segment → **diet_sweep_refs_callee_last_segment** (existing unresolved-only partial index is insufficient); full_ref → **diet_sweep_refs_full_ref**; module_path → **diet_sweep_refs_module_path**; local_name → **diet_sweep_refs_local_name**; requested_name → **diet_sweep_refs_requested_name**; namespace_alias → **diet_sweep_refs_namespace_alias**; target_symbol → **diet_sweep_refs_target_symbol**; new hash_caller_symbol → **diet_sweep_refs_hash_caller_symbol** |
| edges | target_file → idx_edges_target_file_symbol; target_symbol → **diet_sweep_edges_target_symbol**; new id_site_path, id_target_path, id_target_symbol, ref_identity_path → **diet_sweep_edges_&lt;column&gt;**, one index per column unless proven aliases of the existing references |
| dispatch_hints | method_name → idx_dispatch_hints_method; file → idx_dispatch_hints_file; new hash_caller_symbol → **diet_sweep_dispatch_hints_hash_caller_symbol** |
| file_dependencies | file_path → composite primary key; dep_file → idx_file_dependencies_dep_file |
| type_ref_names | name → primary key |
| resolution_config_fields | path → primary key; fields JSON remains unchanged (not an interned reference) |
| backend_file_state | workspace_root → **diet_sweep_backend_file_state_workspace_root**; file_path → idx_backend_file_state_file |
| staging_file_inventory | path → primary key; keep WITHOUT ROWID |
| staging_ref_context | caller_symbol → **diet_sweep_staging_ref_context_caller_symbol**; ref_id is an identity reference, not a string reference |
| view_bindings | file_path → primary key; compressed payload is not a reference into the string table |
| view_file_surfaces | file_path → primary key; same payload rule |
| view_dispatch_files | file_path → primary key; same payload rule |
| view_dispatch_sites | caller_file → composite primary key; language is a vocabulary value, counters and ordinal unchanged |
| view_dispatch_dependencies | file_path → primary key's leading column; dependency → idx_view_dispatch_dependencies_dependency |
| view_unknown_live | caller_file → composite primary key; target_file → **diet_sweep_view_unknown_live_target_file**; target_symbol → **diet_sweep_view_unknown_live_target_symbol** |
| view_module_digest_inputs | input → primary key; digest unchanged |
| view_module_path_targets | target → composite primary key; source → existing source index; declared module path → **diet_sweep_view_module_path_targets_declared** |
| emitted_view_rows | path-valued row_key → `(table_name,row_key)` primary key, with table_name bound separately for each path-keyed table; do not count identity-valued row keys as string references |
| meta, view_sessions, view_usage and views registry/pointer/evidence infrastructure | unchanged; no new string-table references |

New per-form identity fields that duplicate an already stored text field should
use that same interned reference, not introduce redundant ownership. Any format
change that chooses an additional stored string field must add its sweep entry
and plan before writing it. `table_encoding.json.gz` is the authoritative baseline
column census; columns above described as new are obligations for the id slice.

## Write and completion boundaries

Record the old ids of deleted or overwritten references in the same transaction.
REPLACE can delete an unrelated row through nodes' secondary range UNIQUE:
ordinary insert hooks alone do not cover it. Either use recursive delete triggers
with `recursive_triggers=ON`, or explicitly account for every conflicting row.
`stable_rows.rs:6-75` uses both INSERT OR REPLACE and UPDATE OR REPLACE.

For each candidate, check **every** referencing column by indexed equality
SEARCH. Enumerating the candidate table is the only new permitted SCAN. Remove
the candidate even if its string is retained. A completion sweep runs after the
cold-build secondary indexes are recreated, including all proposed sweep
indexes, not during intermediate committed windows. A complete clear of all
referencing tables clears strings and candidates instead of filling candidates.
Staging references remain live until their staging tables are cleared.

## Cross-database identity audit

No production reader attaches a second callgraph database. The only ATTACH in
`crates/aft/src` at the recorded baseline is the module-digest test's finished
fixture copy (`views/materialization/module_digest_tests.rs:255-265`). The raw
ATTACH census is retained in `source_inventory.json.gz:attach`; the full SQL census
also keeps that test statement. Borrowers open one pinned derived database via
`ReadonlyCallGraphStore::open_pinned_derived`; legacy and view readers both use
one CallGraphStore, with manifest_view choosing its mode. Copy/clone publication
is not a cross-database SQL id comparison. Each store's salts therefore remain
local; no cross-store integer join or identity equality is introduced here.
