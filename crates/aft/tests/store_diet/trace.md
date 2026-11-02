# Trace installation and coverage

`baseline-trace.patch` is instrumentation only. It is not applied to production
sources in this change. Apply it to a baseline build, set
`AFT_STORE_DIET_SQL_TRACE` to an output file, and run the same manifest. It opens
an append-only **log**, never a second descriptor on a database file, and writes
one JSON-encoded SQL string per line. There is no reply formatter, resolver,
schema, pragma, order or extraction change in the patch.

`trace_helpers.json` enumerates every raw rusqlite open call under
`crates/aft/src`, its source location and enclosing helper, including test-only
opens. The patch wraps all of them rather than guessing which future route
will be exercised. The important production routes are:

- `db/file_identity.rs`: `IdentityConnection::open`, `open_with_flags`,
  `open_census` and `new`; the flag opener routes through write_accounting.
- `db/lifecycle.rs`: `TrackedConnection::open`, `open_with_flags`,
  `open_path_with_flags`, `open_census` and connection attribution.
- `views/write_accounting.rs::open_connection`: both ordinary SQLite opens and
  opens using the write-accounting virtual filesystem (VFS) wrapper.
- `callgraph_store/mod.rs`: legacy writer `open_at_path_with_root_repair`,
  `open_readonly_connection`, `open_sealed_connection`, `open_readonly`,
  `ReadonlyCallGraphStore::open_manifest_view` and `open_pinned_derived`.
- `callgraph_store/dead_code_projection.rs`: projection read-only open.
- `views/materialization.rs`: blob reader and derived writer, including the
  test probe opener.
- `views/assembly.rs`: derived handle retained through publication.
- `views/generation.rs`: opens used to enumerate publication pointers; completed
  generations with no writer are opened as immutable databases during cleanup.
- `views/registry.rs`, `views/derived_index.rs`, `views/mod.rs`: registry,
  derived-output index and pointer helpers.
- `views/verification.rs`: direct evidence/tree-proof reader and writer opens.
- `db/mod.rs::open_readonly`: shared tracked read-only helper.

These wrappers cover handles opened by helpers rather than merely connections
held by the parity test itself. Blob, registry and pointer metadata use separate
connections, not SQLite ATTACH on a graph reader. They do not contain another
callgraph whose integer ids could be compared to the current graph's ids.

Generate the patch (without applying it):

```
python3 crates/aft/tests/store_diet/trace_patch.py
```

Capture on an isolated baseline checkout after copying the test tool and its
four-line cfg(test) module registration there:

```
git apply crates/aft/tests/store_diet/baseline-trace.patch
AFT_STORE_DIET_SQL_TRACE="$PWD/trace.jsonl" cargo test -p agent-file-tools --lib -- store_diet
python3 crates/aft/tests/store_diet/inventory.py coverage trace.jsonl
```

Trace comparison keeps fixed SQL literals significant and substitutes only bind
parameters. In particular, a query for resolved refs cannot cover a query for
unresolved refs merely because their SQL shapes otherwise agree. Batch members
and minimum/maximum template variants have separate coverage keys. Coverage
fails on an untraced, unexempted variant, on an empty exemption reason, or on a
stale exemption. Exemptions name the unavailable workload/branch rather than
pretending a statement was traced. Goldens remain read-only in traced replay,
so a changed reply cannot be hidden by recording it again.

To review gaps, pass `--report UNCOVERED.json` to `coverage`, then run
`inventory.py draft-exemptions UNCOVERED.json DRAFT.json`. The draft groups
test-only, template-boundary and absent owner workloads with source locations.
Review those owner branches before accepting a draft as trace_exemptions.json;
an unresolved template remains an expansion gap, not a prepared-plan guarantee.
Coverage rejects exemptions for statements that are now traced as well as stale
ids and empty reasons. This prevents an old blanket exemption from hiding a
newly exercised statement.
