# Tool-provider v1 staging

## Release status

**Not released. Slice A is a gated partial implementation; Slice B has not begun.**
The owner approved committing this partial implementation while SUBC 0.28 is
unavailable. Neither lifecycle capability is enabled. This document is an evidence
index, not B8 sign-off. Missing release evidence is a failed/pending release gate,
not a successful skip.

## Bind agreement

The owner/Broca bind agreement for SUBC (the daemon routing protocol) 0.28
specifies `RouteBind.role_versions:
Option<BTreeMap<String,String>>`, declared by Broca on `RouteOpen` and forwarded
by the daemon. It is not a field on `BindIdentity`.

- `None` or a map without `tool-provider`: unchanged Legacy route.
- `{"tool-provider":"v1"}`: ToolProviderV1 route.
- Any other value for that key: `unsupported_role_version`, naming `["v1"]`;
  no legacy fallback. Other role keys are ignored.
- Non-string map values are rejected by the typed transport decoder. The earlier
  scalar `tool_provider` spelling is superseded and is not accepted.

AFT's parser already accepts `Option<&BTreeMap<String,String>>`. SUBC 0.27 has no
such field, so production binds explicitly pass `None`. Wiring the published
0.28 field is required in a follow-up; detecting a role from call pins, session,
harness, or peer name is deliberately forbidden. Tests construct a typed v1
route to exercise admission before transport publication. Those tests do not
constitute real-consumer bind evidence.

## Source and immutable fixtures

Base AFT revision: `23a4af1da239bd97eb8e89a32db6dc62e26dcafa`.
Built package: `agent-file-tools 0.58.1`. Implementation revision is the commit
containing this document and the following files; identify it from git rather
than a self-referential embedded commit hash.

Registry source: `registry+https://github.com/rust-lang/crates.io-index`.
Checksums and release tags are independently pinned in
`crates/aft/tests/fixtures/tool_provider_conformance.json` and checked against
Cargo.lock and Cargo.toml. No commons commit SHA substitutes for registry
provenance. Role `=0.3.0` is normal; conformance `=0.3.0` and harness `=0.1.0`
are dev-only dependencies.

| Artifact | SHA-256 |
| --- | --- |
| `crates/aft/tests/fixtures/tool_provider_catalog.json` | `fbd8f686ad6e8cc5002511f9fb5d78ea4a23475641f1849ad5462d321fc44215` |
| `crates/aft/tests/fixtures/tool_provider_conformance.json` | `7d9936b0323ab6cd69bcc84101d93cd935dc806631e957cf5eada474a81f3a0f` |
| `crates/aft/tests/fixtures/tool_provider_legacy_replies.json` | `ff8f4763ed604ecceebacf4ac7b1d1885d2b42803e86b5e796c72e2af2338934` |
| `crates/aft/tests/fixtures/tool_provider_system_text.json` | `980dfec2799636f1f2e1919dcc3a00f0f65d1cfd9c8adfadc34d83e1cd2f4e2b` |

Catalog fixtures pin full serialized replies, resolved disables, availability,
composition and request inputs. System-text fixtures pin all three literal
Broca variants. The independent declaration/case inventory includes both stages;
Slice B is an expectation only, not an implemented declaration or suite run.

## Slice A local evidence

- `cargo test -p agent-file-tools --lib subc::tool_provider --locked`: 10 passed.
  Includes decoder separation/action-zero refusal checks, metadata bijection,
  byte goldens, disabled native/plumbing spellings, grammar isolation and normal
  read-queue scheduling while database and cold-build capacity remain stalled.
- `cargo test -p agent-file-tools --lib v1_server_completion --locked`: 1 passed;
  real shell commands survive the foreground promotion window and return final
  results without a plugin subscriber.
- `cargo test -p agent-file-tools --test tool_provider_conformance --locked -- --nocapture`:
  3 Rust tests passed. This is **not a passing provider conformance verdict**.
  The real spawned AFT process on SUBC 0.27 executes 29 suite cases: 12 passed,
  3 failed/pending v1 admission, 14 capability-gated skips. Inventory checks
  intentionally preserve these failures until the transport can select v1.
- The real-module binding test checks project/harness snapshots, rebind/restart
  identities and rejects unsupported harnesses. Actual B8 consumer identity is
  still pending; the tested `runner` and `opencode` matrix is not a substitute.
- `RUSTFLAGS="-D warnings" cargo check -p agent-file-tools --tests --target x86_64-pc-windows-gnu --locked`:
  passed. This is a cross-target compile, not local Windows execution.
- `cargo package -p agent-file-tools --locked --allow-dirty`: passed packaging
  and verification. `--allow-dirty` permits this uncommitted staged delivery,
  not a dependency-resolution exception.
- `cargo build -p agent-file-tools --locked`: passed. Normal dependency tree
  includes role 0.3.0 and excludes conformance/harness.
- `cargo fmt --all`: passed. Strict clippy encountered pre-existing lint debt
  (112 lib-test errors); a non-strict run with the existing Windows-permissions
  lint allowed completed. No new tool-provider lint was reported.
- `bash scripts/rust-test-gate.sh`: biome passed; full lib run had 4342 passed,
  1 unrelated timing failure, 49 ignored and 1 filtered. Failure:
  `runtime_drain::tests::watcher_semantic_phase_batches_invalidation_into_one_retain_pass`
  at `runtime_drain.rs:4164`, `assertion failed: !outcome.has_more`.
  Its isolated locked rerun passed.
- `AFT_GATE_PHASES=nextest,watcher,storm bash scripts/rust-test-gate.sh`:
  nextest failed fast: 491 passed, 5 failed, 35 skipped; 2227 not run.
  The failures are existing `callgraph_test` cases:
  `callgraph_aliased_import_resolution`, `callgraph_callers_recursive`,
  `callgraph_callers_empty_result`, `callgraph_callers_cross_file`,
  `callgraph_cross_file_tree`. They report `callgraph_unavailable` /
  `read_only_store_not_built` in this isolated worktree. No parent checkout
  was accessed to satisfy their request to build a persisted store there.
  Watcher/storm phases were not reached. The full repository gate is not green.

### Pending admission failures (exact case IDs)

1. `call_disabled_tool_refused_by_name`: "a call to the disabled tool ended with
   Response(...), not an error frame". Legacy response has `isError: true` and
   structured `tool_disabled` for `aft_outline`; v1 requires an Error frame.
2. `terminal_frame_on_refusal`: "a call to an unserved tool ended with
   Response(...), not an error frame". Legacy structured refusal is
   `unknown_tool` for `conformance.not-a-served-tool`.
3. `schema_pin_malformed_refused`: `schema_pin
   "tp1:echo:839469d5e28de1286cbd75382e5329eb7002d401575fc6c02c4aa17694939b3f"
   ended with Response(...), not an error frame`. The legacy status tool ran
   instead of enforcing the v1 schema pin; real v1 admission must prevent this.

The terminal-frame successes on legacy routes do not prove all requested v1
executor-failure/cancel/deadline/detach outcomes. Complete those transport tests
after the 0.28 bind field is connected.

## B8 freeze and release gates

All of the following remain **pending**; no artifact location, hash, comparison
result, permalink or approval is fabricated:

- Published SUBC 0.28 pin, RouteBind forwarding and real v1 conformance with
  zero failed enabled cases (remove the explicit pending-case expectation).
- Actual consumer harness/session/scope/principal identity; model-name projection
  from bare catalog names; immutable B8 card/config and client revision.
- Raw B8 catalog bytes and frozen manifest artifact locations, SHA-256 hashes
  and exact comparison result; host PowerShell availability and invocation.
- Permalink to the extensibility coordination room's frozen-card bytes message
  and the owner's explicit Slice A release sign-off.
- Linux locked builds/tests, Windows execution including local Windows gates,
  and three-platform real-module conformance.
- OpenCode 2 Docker harness and applicable plugin release assertions.

**Do not begin Slice B integration until these Slice A gates and B8 sign-off
pass.** The forward aft.db migration, durable caller/key ledger, lifecycle
operations, kill/restart recovery and retention gates are not implemented.
Slice B needs its own frozen artifacts and release sign-off before expanding
the declaration.

## Review follow-up: single parse, regeneration and admission mutation proofs

`handle_tool_call` parses wire JSON once into `Value`. Legacy decoding now moves
that value into `RouteRequest`; ordinary large write/edit argument objects are
not cloned. Only an envelope carrying an `op` saves its `params` for the existing
management fallback. `RouteRequest` is untagged but all its string fields are
owned `String`; there is no borrowed-string lifetime change. The base
`23a4af1da239` behavior is pinned explicitly for ordinary tool arguments/host
flags, the existing health-digest management `op`/`params` envelope, and opaque
legacy `call_key`/`schema_pin` tokens, with route response/dispatch controls.

One parse-equivalence exception is explicit: duplicate JSON object keys are
collapsed by `Value` (last value wins). Base byte decoding rejected duplicate
`name` fields; `{"name":"write","name":"status","arguments":{}}` now
decodes as `status`. The test
`legacy_value_decoding_collapses_duplicate_fields_unlike_base_bytes` demonstrates
both outcomes. Ordinary unique-key envelopes and untagged `bg_events` precedence
are unchanged.

Regenerate all catalog replies from the committed request/config inputs with:

```sh
cargo test -p agent-file-tools --lib regenerate_tool_provider_catalog --locked -- --ignored
```

The generator is explicitly ignored during verification; it does not overwrite
expectations during normal test runs. The catalog byte-drift assertion prints
this command. Running it on the unchanged descriptions produced no fixture diff.
Review generated byte/digest changes before committing; updating the fixture is
not itself proof that a schema or semantics change is approved.

### Captured admission reds

Each control was staged before mutation, marked `NON-VACUITY BREAK`, run alone,
and restored with `git checkout -- <path> && touch <path>`. Each named test was
the only selected test, with **0 passed, 1 failed, 4400 filtered**; other tests
were not run in these targeted red runs. Final restored subc verification runs
all five together. No mutant is retained in the source.

- Session guard disabled in `tool_provider.rs`: non-empty diff **1 insertion,
  1 deletion**; restored diff empty. Test
  `subc::tool_provider::tests::admission_requires_session`:
  `called Result::unwrap_err() on an Ok value: ()`.
- Entire served-schema argument validation bypassed in `tool_provider.rs`:
  non-empty diff **2 insertions**; restored diff empty. Test
  `subc::tool_provider::tests::admission_refuses_arguments_outside_served_schema`:
  `called Result::unwrap_err() on an Ok value: ()`.
  An earlier narrower mutation disabling only the unknown-key guard also went
  red (`left: String("arguments"), right: "not_served"`), but JSON Schema still
  refused; the complete bypass above proves the requested admission property.
- Name-disable guard disabled in `tool_provider.rs`: non-empty diff
  **1 insertion, 1 deletion**; restored diff empty. Test
  `subc::tool_provider::tests::admission_disables_companions_regardless_of_task_spelling`:
  `assertion left == right failed: bash_status; left: "invalid_request";
  right: "tool_disabled"`. The restored test covers all three companions with
  `task_id`, `taskId`, and both spellings, before argument validation.
- Malformed schema pins skipped at both validation sites in `tool_provider.rs`:
  non-empty diff **3 insertions, 2 deletions**; restored diff empty. Test
  `subc::tool_provider::tests::admission_refuses_malformed_schema_pin`:
  `called Result::unwrap_err() on an Ok value: ()`.
  The first restore hit a concurrent git index lock; a subsequent restore
  succeeded and confirmed the empty diff before any further mutation.
- Legacy provider-operation firewall removed in `subc/mod.rs`: non-empty diff
  **2 insertions, 1 deletion**; restored diff empty. Test
  `subc::tool_provider::route_tests::legacy_provider_calls_are_unsupported_without_actions`:
  `tool.call: expected Error unsupported_operation, got Response; actions=1;
  tool.withdraw: expected Error unsupported_operation, got Response; actions=2`.
  This demonstrates both operations dispatch when their refusal is removed.

Restored review verification:

- `cargo fmt --all`: passed.
- `cargo test -p agent-file-tools --lib subc --locked`: **320 passed,
  0 failed, 7 ignored**; all five admission tests and legacy identity tests passed.
- `cargo test -p agent-file-tools --test tool_provider_conformance --locked -- --nocapture`:
  **3 Rust tests passed**; actual provider verdict remains **12 passed,
  3 failed/pending v1 admission, 14 capability skips**, unchanged from above.
- `RUSTFLAGS="-D warnings" cargo check -p agent-file-tools --tests --target x86_64-pc-windows-gnu --locked`:
  passed.
- Scoped `aft_inspect` could not finish rust-analyzer indexing; the compiler and
  Windows checks are the authoritative diagnostics for these changes.

The review rerun of `bash scripts/rust-test-gate.sh` passed biome and the full
workspace lib/bins phase, then nextest failed fast with **280 passed, 1 failed,
35 skipped; 2442 not run**. The unrelated failure was
`gh_shim_discovery_retry_succeeds_when_only_the_first_attempt_times_out`:
`governance probe timed out after 2000 ms at catalog_list (daemon may be busy;
host load?)`. Its isolated locked rerun passed (1 test). Watcher/storm phases
were not reached. The complete repository gate is not claimed green.
