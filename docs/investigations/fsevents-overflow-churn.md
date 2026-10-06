# macOS FSEvents overflow/client churn

## Findings and limits

The supplied daemon evidence correlates `user_dropped` overflows with new
FSEvents clients (316 registrations and 310 overflow warnings in one minute).
Correlation alone does **not** establish that the recursive project watcher was
rebuilt. A separate config-file fallback watch has a reproducible per-overflow
reattachment bug. The recursive watcher does not have that bug when its exclusion
paths remain unchanged.

Measurements below were made on arm64 macOS with Rust/Cargo 1.99.0, using temporary
fixtures and a throwaway HOME and mkdir'd XDG data/config/state/cache directories.
CARGO_HOME and RUSTUP_HOME were retained. AFT_STORAGE_DIR was unset. The production
daemon, its storage, its log, and other repositories' worktrees were not accessed.
Consequently the supplied production counts are input evidence, not a new
measurement, and the exact flooding paths in pid 48340 cannot be established from
this investigation. The excerpt does not include actual `candidates_dropped` or
`top_prefixes` values; the eight selected paths alone cannot identify the culprit.

## 1. Which operation allocates a stream?

There are three relevant owners:

* `watcher_backend/fsevents.rs::ProjectWatcher`: one recursive native stream;
  matcher-generation publication replaces it only if the sorted exclusion paths
  change. An overflow does not terminate its filter thread.
* Project auxiliary paths (Git metadata, external ignores, and existing
  `.cortexkit`): previously a `notify::recommended_watcher`. Every `watch()` on
  notify 8.2.0 stops and recreates its combined stream, including while adding
  the initial path list. This is startup/path-list churn, not a per-overflow
  rebuild in the project backend.
* `config_live.rs::run_config_file_watch`: a separate fallback for a config
  directory not covered by the project watcher. If `.cortexkit` is absent, it
  watches the project root until that directory appears. It previously forced
  reattachment whenever an event named its watched directory, regardless of the
  event kind. FSEvents drop sentinels name precisely that root. The subsequent
  `DirAttachment::attach` unwatches and watches it again. notify's `watch_inner`
  calls `run`, which calls `FSEventStreamCreate`.

Thus the demonstrated per-drop path is:

```text
root-naming UserDropped sentinel
  -> run_config_file_watch
  -> force_reattach
  -> DirAttachment::attach
  -> unwatch + watch
  -> notify::FsEventWatcher::watch_inner -> run -> FSEventStreamCreate
```

The main overflow drain instead invalidates verification/cache state, rebuilds
the ignore matcher, and reconciles the indexes. It does not reinstall the project
watcher. A genuinely changed exclusion set can still cause a replacement, using
the existing event-cursor replay handoff.

### Counted regression, not just source inference

`config_live::tests::watcher_config_user_dropped_rescans_without_creating_streams`
uses the real native stream owner and the same event/attachment handler as the
config loop. Eight simulated root-naming drops originally allocated **8 additional
streams**. The first red assertion was `left: 8, right: 0`. After the fix they
allocate **0 additional streams** and request **8 content checks**.

An LLDB breakpoint on **the framework's `FSEventStreamCreate`**, not on our
counter, corroborated the measurement:

| Run | Native create hits (including initial attach) | Purge hits |
| --- | ---: | ---: |
| Fixed config regression, eight drops | 1 | 0 |
| Same regression, original per-drop reattach restored | 9 | not measured |

`watcher_user_dropped_rescans_keep_recursive_stream_with_unchanged_exclusions`
injects native UserDropped/MustScanSubDirs flags through the callback into a live
recursive backend and the real filter. All **8** typed rescan requests are
received and acknowledged; each acknowledgement publishes a new matcher
generation. The creation counter remains **1** (zero replacements). This test
stayed green during the config-reattach mutation: the two owners are distinct.
Existing drain tests additionally check that overflows arriving during a state
walk coalesce into one follow-up without blocking the filter.

## 2. Who issues the purge RPC?

It is an explicit call in the locked **notify 8.2.0** macOS backend, not an
implicit framework side effect of `FSEventStreamRelease`:

```text
FsEventWatcher::stop / unwatch_inner / watch_inner / Drop
  -> CFRunLoopStop + join runloop thread
  -> notify::fsevent::FsEventWatcher::run closure
  -> FSEventStreamStop
  -> FSEventsGetCurrentEventId
  -> FSEventStreamGetDeviceBeingWatched
  -> FSEventsPurgeEventsForDeviceUpToEventId  (fsevent.rs:489)
  -> FSEventStreamInvalidate -> FSEventStreamRelease
```

A small scratch executable using exactly `notify = "=8.2.0"` performed one
nonrecursive watch/unwatch under the temporary HOME. LLDB stopped in the purge
function on thread `notify-rs fsevents loop`; its caller was
`notify::fsevent::{impl#5}::run::{closure#0}` at `fsevent.rs:489:21`. Continuing
printed:

```text
dev 0 () : purging events up to event id 4108593955
FSEventsPurgeEventsForDeviceUpToEventId: f2d_purge_events_for_device_up_to_event_id_rpc() failed: 5
```

The native stream owner already stops/invalidates/releases without purging.
macOS config and project auxiliary watches now use that owner too. Auxiliary
paths are batched into a single stream rather than starting a new notify stream
per initial path. No package or lockfile change is needed. Other platforms retain
their existing notify config watcher.

## 3. What can flood, and what does the eight-slot budget miss?

A nonrecursive FSEvents watch is still recursive **inside the framework**.
notify's nonrecursive mode filters callback paths in userspace; it does not
install the recursive project watcher's eight exclusions. The config fallback
watching a fresh worktree root can therefore receive build traffic that the
project stream successfully excludes. Altering the project slot ranking cannot
fix this separate unexcluded stream.

The live fixture
`watcher_unexcluded_auxiliary_stream_receives_excluded_build_burst` puts an
excluded stream and an otherwise equivalent unexcluded stream on the same root.
Writing 200 files under `target/debug` and one source control file yielded:

```text
target/debug burst: excluded=0 unexcluded=400 excluded_kept=2 unexcluded_kept=2
```

These are translated raw event counts (a file can generate multiple kinds), not
200 induced overflows. The control proves both streams were live. This shows
that an unexcluded auxiliary stream is exposed even when `target` has a slot; it
does **not** prove that production's omitted `target` was the flooding path.

For a production diagnosis, join each root's `candidates_dropped` with its
`top_prefixes` and `ring_span_ms` from `watcher overflow` lines, plus its initial
`watcher exclusions` line and matcher generation. An ignored dropped candidate
matching a high-volume prefix is evidence of a slot miss. A selected candidate
matching that prefix suggests a separate/unexcluded stream or a coverage issue,
not automatically a bad ranking. Prefix counts are recent delivered events,
not a census of events already lost; an empty ring is not evidence of no flood.

Recommendations supported by the existing ranking/tests:

* Keep the fixed root `.git` slot when `.git` is a directory. A linked worktree's
  `.git` is a file, not a build subtree deserving a slot; its important metadata
  is watched separately.
* For a fresh mixed Rust/JS root, retain a manifest-gated, ignored `target` seed
  alongside `node_modules` and a build-output representative. Do not let eight
  copies of node_modules or dist crowd every other ecosystem out before any
  observations exist. The base code already implements representative-first
  breadth, tested by `exact_path_seed_keeps_rust_and_js_covered_when_node_modules_fill_every_slot`
  and `fresh_mixed_rust_node_root_reserves_ecosystem_exclusions_first`.
* After an overflow, prefer a **measured ignored** dropped directory while
  retaining breadth for existing build/dependency representatives. The base
  code already reserves slots for both classes. Never exclude source paths
  merely because they are busy.
* If the real log shows `target/debug` or another ignored output among the
  dropped candidates/top prefixes, promote that candidate rather than another
  unobserved TS output. If the auxiliary root stream is the source, consider
  applying a matching exclusion plan to it in a follow-up; changing the main
  stream's ranking alone buys nothing there.

No ranking policy was changed without the missing production attribution data.

## Fix, correctness, and observability

Drop events always request a config content check, even if their paths are empty
or outside the nonrecursive scope. They never force reattachment just for naming
the root. Directory create/remove/rename events retain the explicit reattach
safeguard for inode reuse. Identity checks on every event/tick still detect real
directory replacements after events were lost. Backend errors still reattach.

The recursive overflow rescan and event-ID replay handoff are unchanged. No
rebuild rate limit is needed for stable exclusions: the demonstrated allocation
rate is zero per drop. Existing drain burst coalescing remains in place.

`watcher.fsevents_stream_creations_total` is now serialized in per-root status and
health. It counts successful `FSEventStreamCreate` allocations, including initial
attachments, exclusion replacements, auxiliary paths, and the root's config
fallback. Allocations that subsequently fail to start still count. It is a
process-lifetime total; compare deltas against `overflows_total` and
`rescans_user_dropped_total`, rather than interpreting it as an instantaneous
rate. Non-macOS roots report zero. The process-global user-config watch is not
owned by one project root; standalone context-owned user watches are attributed
to that context's root.

Mutation proofs restored the old reattach predicate (eight extra creations),
suppressed native allocation accounting (initial count became zero), and removed
the rescan exception from nonrecursive filtering (the out-of-scope drop vanished).
Each targeted test went red, and staged live source was restored afterward with
an empty unstaged diff. No existing test was rewritten to accept changed behavior.

## Verification

All Cargo test commands used the temporary HOME/XDG setup described above.

* `cargo test -p agent-file-tools --lib -- watcher`: 100 passed, 8 ignored.
* `cargo test -p agent-file-tools --test integration -- watcher`: 22 passed.
* `cargo test -p agent-file-tools --test watcher_integration`: 29 passed on macOS,
  including config live reload, Git branch switching, and ignored-event floods.
* `cargo test -p agent-file-tools --lib -- config_live --test-threads=1`:
  33 passed, including replacement directories and late directory creation.
* Health serialization regression: 1 passed.
* Live backend/exclusion probes: the combined ignored run passed five of seven;
  the handoff probe timed out waiting for the old callback, and the tiny Cargo
  build probe observed 18 startup/raw events instead of zero. Running these two
  failing probes individually passed (handoff replay delivered the withheld
  write; Cargo build delivered zero raw events). A parallel config unit run
  likewise missed a live callback once; its serial run passed all 33 tests.
  These transient live-service failures are not hidden by widening assertions.
* `cargo fmt --all -- --check`: exit 0, rustfmt 1.10.0-stable.
* `RUSTFLAGS="-D warnings -A deprecated" cargo check --target x86_64-pc-windows-gnu --tests -p agent-file-tools`:
  Finished successfully with Rust/Cargo 1.99.0 and MinGW GCC 16.2.0. The first
  attempt could not locate the cross compiler; the same gate passed once the
  compiler was available. This is compile-only, not Windows runtime evidence.

For the native hit counts, run LLDB against the lib test executable with
breakpoints on `FSEventStreamCreate` and `FSEventsPurgeEventsForDeviceUpToEventId`,
auto-continue each breakpoint, run the exact config regression, and inspect
`breakpoint list` after exit. This independent check matched the production
counter instead of comparing a proxy to itself.
