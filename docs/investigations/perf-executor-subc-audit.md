# Executor and subc performance audit follow-up

Scope: sections 6 and 7 of the external performance audit, checked against base
`14fb55e3b0e8e19cf7f58d50615ce9aefc1aa3a8`. The cross-cutting patterns informed
the changes: debounce before census, bound request work, avoid control-path lock
waits, move live reconciliation off the frame thread, reuse immutable schema
work, and wake deferred consumers from their producers.

The interrupted slice's three commits were replayed onto main
`2772a315f207a1787be34fba3557ad2013d0f565`. The catalog-cache conflict was
resolved by retaining main's head/worker/reader presets and their live disable
and host filters, with one immutable catalog per preset. Admission continues
to validate against the worker superset, as main did before caching.

## High findings: measured and fixed

The counts below came from executable tests, not wall-clock comparisons. The
before/after counter tests were run against the old work paths and failed with
the stated before counts. The lock tests are ordering proofs: a second thread must answer
while the test still owns the manager mutex. Their timeout is only a deadlock
guard, not a latency benchmark. New producer-wake tests were also mutation-tested.

| Finding | Verdict | Work before → after | Test name(s) |
| --- | --- | --- | --- |
| 6.1 Status census before debounce | confirmed; fixed | 12 default-session snapshot builds per burst → 1 | `status_signal_burst_builds_one_snapshot` |
| 6.2 Request watcher drain exhausts queue | confirmed; fixed | 4,097 applied deletion paths per request → at most 2,048 (one existing budgeted slice); all 4,097 eventually applied | `request_watcher_drain_applies_at_most_one_slice` |
| 6.3 LSP mutex on request drain/status bar | confirmed; fixed | 1 blocking manager acquisition in each API → 0; contended drains retain demand for the next scheduling turn without an immediate zero-progress requeue; bar publication skips the turn | `request_lsp_drain_defers_a_held_manager_without_waiting`, `contended_lsp_maintenance_does_not_requeue_without_progress`, `status_counts_do_not_acquire_a_held_lsp_manager` |
| 7.1 Standing reconciliation on frame loop | confirmed; fixed on hot path | 12 frame-thread reconciliation callbacks per 12 tick submissions → 0; real git-root fixture: 2 live root resolutions per reconciliation → 1 | `standing_tick_requests_do_no_frame_thread_reconciliation`, `reconciliation_resolves_each_live_root_once` |
| 7.2 Catalog/digests and schema compilation per admission | confirmed; fixed | 12 full catalog builds + 12 validator compilations per 12 warm `read` admissions → 0 + 0 | `warm_admission_does_not_rebuild_catalog_or_validator` |
| 7.3 Deferred completion polling | confirmed; fixed for completion discovery | 12 idle polls after registration → 0; each real inspect/navigation producer emits 1 completion wake | `deferred_registry_idle_turns_do_not_repoll_and_completion_wakes`, `deferred_inspect_producer_notifies_completion`, `deferred_navigation_producer_notifies_completion` |
| 7.4 Repeated signed-manifest verification/state writes | confirmed; fixed | 3 signature verifications + 3 last-valid record rewrites per 3 resolutions → 1 + 1 | `repeated_manifest_resolution_verifies_and_writes_once` |

### Boundaries and unchanged contracts

- Transport-owned `Arc<AppContext>` handles register a weak status-builder
  handle. Debounced emission upgrades it only when a surviving signal has a
  subscriber. It does not keep a retired root alive. The library's stack-owned
  context and already-built `StatusEmitter::signal` APIs retain their old eager
  behavior. Explicit status requests still build a fresh snapshot.
- The watcher fixture represents a checkout deletion burst, so the 4,097 paths
  legitimately no longer exist on disk. The test checks both the per-request
  bound and complete eventual application. The existing path/time budgets and
  continuation machinery are unchanged; no visible result list cap was added.
- Contended diagnostic counts are unknown, not fabricated zeros. Explicit
  status requests can still report independent Tier-2 values, but bar publishers
  skip a contended observation entirely: they neither clear the fleet's last
  segment nor emit a temporary `E? W?` bar nor consume the emission fingerprint.
  Status-change pushes retry on the next signal. Normal missing-producer values
  still use `?`; genuine absence is not confused with temporary lock contention.
- Standing work uses one worker per connection with one queued follow-up slot,
  not a thread per tick or a new timer. Startup reconciliation remains direct.
  Bind and background reconciliation are serialized. A pass still resolves live
  filesystem/git identity and opens the existing tracked database: no stale root
  cache was introduced. The removed duplicate resolution also prevents mixing
  two root observations in one durable record. No extra SQLite descriptor was
  added.
- Embedded schemas, digests, semantics, and lazily compiled validators are
  immutable for the lifetime of the executable. Host PowerShell availability and
  disabled-tool policy are still evaluated at each catalog/admission boundary.
  Existing independent catalog/digest goldens are unchanged.
- Deferred completion generation is captured **before** polling; a completion
  racing the poll therefore remains discoverable. Registration forces an initial
  probe even if a producer finished before registration. Producer guards wake on
  normal return and unwinding. Navigation retains its 100 ms independent deadline
  check, including under sustained frame traffic; inspect timeout terminals come
  from its producer. Cancellation, detach and shutdown terminal handling are
  unchanged. Standalone's existing wake/poll path is not redesigned here.
- The manifest memo holds only one exact envelope/trust-set pair per shim thread.
  Every resolution still reads the live installed artifact and checks its presence,
  version high-water mark, schema floor and issue time. Identical last-valid bytes
  are checked against disk rather than a remembered write. Missing/corrupt local
  records are repaired. The memo does not cache governance, routing or user config.
  Existing signature, trust rotation, rollback and captured wire goldens pass.
- No ranking-fenced search files, parser, LSP round-trip implementation, durability
  policy, tool arguments, manifests, or lockfiles were changed.

## Read behavior during an unapplied watcher burst

The request path applies at most one 2,048-path slice, including its existing
time budget. Remaining paths stay in the watcher continuation for maintenance
or the next request. A ready producer does not mean that it has reflected that
continuation. The admission-time pending observation is retained through response
rendering even if maintenance finishes concurrently.

| Read tool | What it can answer before the rest is applied |
| --- | --- |
| `read` | Reads live file bytes (or directory entries), independent of watcher invalidation. It does not need a pending-index gap. |
| `grep` | Ready trigram candidates can omit newly matching/new files; candidate contents are verified from disk, which cannot repair an omitted candidate. The filesystem fallback is live but conservatively carries the same gap while a burst is pending. |
| `glob` | A ready index can omit new paths; its on-disk presence check only removes deleted paths. A filesystem fallback remains live. |
| `search` | Lexical and semantic producers may still reflect earlier contents or corpus membership. |
| `outline`, `zoom` | Parse/read the requested files, but symbol caches and project discovery have not necessarily been invalidated for remaining paths. |
| `callgraph` | Cached graph or published checkout generation may omit remaining changes; the checkout query wait knows only paths already recorded by the drain. |
| `inspect` | Fresh scans and LSP observations may complete while derived Tier-2 producers/graph inputs still omit remaining changes. Deferred inspect retains its admission observation too. |
| `conflicts`, `ast_search` | Git conflict discovery and direct AST file scans use live inputs, not the deferred indexes. |

Index/cache-backed reads carry `complete: false`, a `watcher_pending` gap, and
the agent-visible trailer “Watcher changes pending” while the admission observed
unapplied changes. This is a freshness gap, not a result-list cut; it invents
neither a path count nor a completion deadline. The integration fixture builds
a real index, then creates a new file beyond the first slice: grep/glob miss it
and disclose that gap; read returns its live bytes; after full application both
indexed tools find it and the trailer disappears.

LSP contention originally produced `processed: 0, has_more: true`, which the
maintenance completion unconditionally requeued. A held manager therefore
spun the lane without progress. Immediate LSP requeue now requires progress;
the ordinary scheduling probe still retries on the next maintenance tick.
No event is removed on contention, and successful partial drains still requeue.

### Revision counts and non-vacuity checks

| Boundary | Before → after | Executable proof |
| --- | --- | --- |
| Burst freshness | 0 disclosures for ready-index false negatives → all 7 index/cache-backed read tools disclose a gap; the raced grep response retains its gap after maintenance finishes | `request_watcher_burst_discloses_unapplied_index_changes`: 4,097 paths, at most 2,048 applied on admission; grep/glob each miss the new file before application and each find it afterwards; read returns live bytes throughout |
| In-flight continuation | Empty slot hides active application → pending remains visible until the active drain finishes | `watcher_query_reports_in_flight_changes_until_application_finishes` parks a real apply phase after dequeue |
| Zero-progress LSP completion | 1 immediate LSP requeue → 0, with 1 retry admitted on the next scheduling turn | `contended_lsp_maintenance_does_not_requeue_without_progress`: all 257 queued events survive contention, then drain as 256 + 1; the progressing first batch still requests 1 continuation |
| Contended bar/fleet publication | 1 temporary unknown bar/quiet fleet clear → 0 publications; the changed D count is emitted on retry | `status_counts_do_not_acquire_a_held_lsp_manager`: starts from E0/W0, holds the mutex through a separate thread's count, snapshot and publisher calls, then observes D9 after release |

All three requested regressions failed before the fixes. Four temporary
implementation breaks then failed only their exact-name selected tests:
suppressed watcher disclosure, hidden active-drain demand, unconditional LSP
requeue, and restored contended bar/fleet publication. The existing bounded
watcher and nonblocking LSP tests remained green in that same mutated build.
The staged live state was restored with checkout and touch; unstaged diff stats
were empty both before mutation and after restore (three files, 10 insertions
and 7 deletions while mutated).

The status source seam fences needed to recognize two truthful accessor sites:
the agent bar still renders only in `status_bar_line`, but the fleet publisher
now also distinguishes a busy observation from missing producers before taking
the legacy projection. The old source spelling asserted a legacy accessor
instead. Their transport/formatter/inspect exclusion and projection claims
remain intact. A fifth break inserted an actual third count read in response
finalization: only
`truthful_counts_and_inspect_payload_stay_out_of_agent_response_transport_seams`
failed, while the other three seam tests stayed green. That two-line mutation
was restored to an empty unstaged diff too.

Final current-main verification passed 717 scoped lib tests (10 ignored), 121
binary tests, 172 list-envelope tests, 11 request/read/status integrations, 6
real-provider conformance tests (including all presets), 3 standing acceptance
tests, 13 captured gh wire goldens, and 4 status seam tests. One real-daemon
provider e2e remains explicitly ignored without `AFT_E2E_SUBC_BIN`. Strict
Windows compilation of all configured test targets and rustfmt passed.
Rust-analyzer inspection reported unknown diagnostics after its indexing budget;
Cargo supplied the authoritative compile checks. macOS linkers warned that the
large pre-existing test binaries' unwind tables exceed the compact-unwind limit.

## Remaining findings: current-code verdicts and scope decisions

These optional Medium/Low items were inspected but not optimized. Counts here
describe current code structure, **not** newly measured benchmark improvements;
the before/after column is unchanged because no fix is claimed. A dash means no
new counter test was added. Paths below are relative to `crates/aft/src`.

| Finding | Verdict / current evidence | Work before → after; decision | Test |
| --- | --- | --- | --- |
| 6.4 View health opens SQLite/DDL/scans | confirmed: `context.rs::view_health_snapshot` opens `PathStatusStore` then calls `summary` | 1 open/summary per enabled view snapshot, unchanged; deferred: requires exact per-view connection/publication lifecycle, not another fd on a live SQLite set | — |
| 6.5 Duplicate tool payload | confirmed: `commands/tool_call.rs::response_with_text`; non-core fallback in `subc_format.rs` serializes the response | structured payload plus rendered copy, unchanged; skipped: removing either changes the public response contract | — |
| 6.6 Status-bar counts twice/before cadence | confirmed when a fleet publisher is installed: `publish_fleet_status` uses `status_bar_counts`, `status_bar_line` uses truthful values before `should_emit_status_bar` | up to 2 accessor calls, unchanged; skipped: separate fleet/host cadence semantics and authoritative cache accounting | — |
| 6.7 Flattened request buffering/argument clone | confirmed: `protocol.rs::RawRequest`, `commands/tool_call.rs` clones arguments | flatten buffer + owned argument copy, unchanged; skipped: protocol ownership/duplicate-key semantics need dedicated compatibility tests | — |
| 6.8 Cold limiter/view slot polling | confirmed: `cold_build_limiter.rs` sleeps 100 ms; `executor/view_publication.rs` retries permit after cancellation-aware 25 ms wait | periodic attempts unchanged; skipped: shared admission-priority/cancellation wake redesign | — |
| 6.9 View commit under JOBS mutex | confirmed: `executor/view_publication.rs` holds JOBS through `commit_view_update` and sealing | 1 global critical section through commit, unchanged; skipped: atomic supersession/publication guarantee needs a separate protocol change | — |
| 6.10 Watcher index I/O/write lock and fresh memo | confirmed in `runtime_drain.rs::apply_watcher_slice` | existing per-slice update work unchanged; deferred: root-cause index API is in ranking-fenced `search_index.rs` | — |
| 6.11 Batch edit rescans and rereads | confirmed: `commands/batch.rs` repeatedly splices and collects source lines, then reads formatted/validated content | per-edit scans/splices and rereads unchanged; skipped: edit/format/validation transaction needs a dedicated correctness corpus | — |
| 6.12 Two request drains/retention registry clone | confirmed with qualification: `main.rs` second drain only when no input is queued; `signal_bg_registries` precedes retention's due check | 1–2 drains/request and registry clone unchanged; skipped: preserve the post-response maintenance opportunity; each watcher drain is now bounded by 6.2 | — |
| 6.13 Deferred threads/validation polling | confirmed: `main.rs` spawns workers and uses 10 ms validation-gate polling | thread/request and validation polls unchanged; skipped: standalone offload/validation ordering is separate from subc completion signaling | — |
| 6.14 Alert text copy/scan before alert availability | confirmed: `response_finalize.rs::attach_alert_block` copies text then scans the reminder marker before `alerts.finalize` | one text copy/scan unchanged; skipped: alert ordinal/state transitions need separate proof before reordering | — |
| 6.15 Alert state clone | confirmed: `alert_state.rs::accept_batch_at` clones state for atomic staging | one state clone/batch unchanged; skipped: clone implements rollback on invalid authoritative batches | — |
| 6.16 Three gap attachments/O(n²) dedup | not a real cost **as phrased**: three finalizer entry points are not three unconditional sequential calls; nested adapters can still repeat attachment, and `gaps.contains` is linear | entry-point-dependent calls and quadratic gap dedup unchanged; skipped: checkout timeout-only path, preserve nested adapter behavior | — |
| 6.17 Scheduler linear bookkeeping under lock | confirmed: executor queues use `.position` on lane/job order and actor-order retention | linear queue scans unchanged; skipped: queue fairness/configure coalescing redesign is invasive | — |
| 6.18 Formatter clones | confirmed: `subc_format.rs` clones values before typed list-envelope deserialization | one envelope clone at each relevant renderer unchanged; skipped: broad rendering ownership change for Low severity | — |
| 7.5 Repeated origin git spawns | confirmed: `gh_shim.rs::origin_remote` is reachable through repeated target/checkout binding resolution | one child per origin lookup; invocation count remains branch-dependent, unchanged; skipped: must preserve live target/checkout observations across governance probes | — |
| 7.6 Per-image reqwest client/serial downloads | confirmed: `github_read/attachments.rs` builds TLS/client inside `download` | up to 8 client builds per vision read, unchanged; skipped: optional batching/TLS-trust lifetime work after the High fixes | — |
| 7.7 Sequential gh fetches/cache fallback-only | confirmed: `github_read/fetch.rs` and `cache.rs` explicitly require live fetch before fallback | issue/PR fetch processes unchanged; skipped: cache-first is a user-visible freshness change, not an output-preserving optimization | — |
| 7.8 Comment ordinal live reread | not a real avoidable cost without changing correctness: `commands/github_comments.rs` derives the ordinal from the complete live discussion after write | 1 live reread unchanged; no fix: ordinals cannot safely be inferred from the create response or stale cached comments | — |
| 7.9 PowerShell availability scan | confirmed: subc catalog/admission still call `powershell_available` | PATH availability checks unchanged; skipped: executable installation/removal invalidation must be exact | — |
| 7.10 Relay route open/close and close mutex | confirmed: `gh_shim_relay/mod.rs::route_request` releases its last lease via `close_handle` under the holders lock | last lease closes route, unchanged; skipped: comment documents the cache-close/reopen ordering guarantee | — |
| 7.11 Relay catalog listing | confirmed: `gh_shim_relay_client.rs` calls `catalog_list` when resolving the route holder | full catalog listing per discovery, unchanged; skipped: no exact daemon module-change invalidation contract established | — |
| 7.12 Health rollup/metrics size clone | confirmed, with earlier mitigation: `subc/health.rs::HealthRollupWorker` runs off frame thread, but `metrics_encoded_bytes` still clones the metrics map | 3 s worker rollups and map clone unchanged; skipped: optional health-budget serialization work, not repeated frame-loop census | — |
| 7.13 Repeated user config reads | confirmed: shim hard-off checks and rung/config resolution independently read user config | branch-dependent repeated reads unchanged; skipped: do not freeze operator hard-off across a governance wait in this slice | — |
| 7.14 Writer copies/one write per frame | confirmed: `subc/mod.rs::write_frame_contiguous` reuses a buffer but copies the body and writes each complete frame | 1 body copy/write_all per frame unchanged; skipped: cross-frame coalescing changes buffering/backpressure | — |
| 7.15 Reliable push serialization/channel | confirmed: `subc/push.rs::fan_out_reliable_push_frame` calls `try_send_push_frame` per channel (lossy fanout already shares a serialized body) | one serialization per reliable recipient unchanged; skipped: replay/retention representation change for optional severity | — |
| 7.16 Unbuffered split read half | not established as a real cost: split remains unbuffered, but dedicated reader owns whole-frame reads; syscall impact requires transport instrumentation | no measured regression or fix claimed; skipped speculative Low finding | — |
| 7.17 Value parse/typed conversion and clone | confirmed: `subc/mod.rs::handle_tool_call` decodes Value then v1 typed request | two representations and envelope ownership copies unchanged; skipped: legacy duplicate-field behavior is explicitly pinned by existing tests | — |
| 7.18 Relay token cache never evicts | confirmed with qualification: `drop_token` removes rejected tokens, but expiry alone does not prune historical keys | one retained key/agent-session until explicit removal, unchanged; skipped: optional bounded token ownership policy | — |
| 7.19 Retry allocations/ticket scans/image canonicalization | confirmed: `subc/push.rs` collects route keys for retry draining; shim image comparison canonicalizes; tickets use linear lookup | route vector/ticket scan/image probes unchanged; skipped: several unrelated Low paths deserve separate counters and lifecycle tests | — |

## Mutation and compatibility evidence

The live implementation was staged before mutations. `git diff --stat` was empty
before mutation, non-empty during mutation (nine source files, 29 insertions and
53 deletions), and empty after `git checkout -- <specific paths> && touch <paths>`.
Every break was explicitly marked, and no mutation remains in the tree.

Eleven separate exact-name test runs each failed only their named test: the seven
High rows' counter/ordering tests plus the two actual producer-wake tests. The
mutations restored eager census, exhaustive draining, blocking LSP acquisitions,
duplicate root resolution, synchronous standing work, per-call catalog/validator
builds, repeated manifest verification/rewrites, unconditional completion polling,
and suppressed inspect/navigation producer wakes. Three independent controls
remained green in the same mutated build:

- `full_catalog_goldens_and_digest_only_have_exact_identity`
- `manifest_memo_rechecks_live_bytes_rollback_and_repairs_local_state`
- `watcher_drain_requeues_until_all_events_are_applied`

After restoration, all affected library suites, binary tests, list-envelope tests,
status/request integration tests, real-module tool-provider conformance, standing
acceptance, gh captured wire goldens, watcher integration, formatting, and the
strict Windows compile gate passed. The first Windows run caught a now-test-only
`PendingSubcResponses::is_empty` method; it was correctly test-gated and Windows
and subc checks were rerun. Rust-analyzer inspection timed out while indexing; the
actual Cargo compilation/tests supplied authoritative diagnostics instead. The
`commands::lsp_navigation::` lib filter contains no in-module tests; its real
producer/cancellation behavior is covered by the nonzero subc test suite.
