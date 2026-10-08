# Views storage growth, 2026-10-08

Base: `2b391db0e046c74afcd6dfbe710aa94fbe73fbc5` (AFT 0.58.2).

## Safety and conclusions

**No live storage was modified by this investigation. No SQLite file was opened,
read, copied, hashed, or queried**, including `pointer.sqlite`, derived databases,
blob databases and `aft.db`. Measurements used `lstat`, directory enumeration,
plain reads of JSON / `.ref` / `.git` metadata, and PID existence checks. Tests ran
on Linux with throwaway HOME and XDG directories outside any checkout, with
`AFT_STORAGE_DIR` unset. The older `scripts/storage-retention-report.py` was **not**
run: it queries live SQLite and is unsuitable for this brief's safety constraint.

Findings:

1. `views/c0c39eea197fcc68` is the **main prefrontal checkout's scope**, not the
   worktree collection bearing that name. Both the canonical-path SHA-256 and
   `retention/roots/c0c39eea197fcc68.json` identify
   `/Users/ufukaltinok/Work/Projects/CortexKit/prefrontal`; its artifact family is
   `be627d40119a995e`. The worktree collection existed with 417 entries in the later
   sample, but is not the root referenced by this view.
2. At 09:20 UTC the folder had **11 generation databases**, not 40 databases.
   The 40 top-level entries included manifests, zero-byte trigrams, coordination
   directories, the pointer file set and a legacy `derived.sqlite`.
3. Ten generation databases had no assembly pin, query marker or `.ref` reference.
   Only generation 27 had a live reader, PID 83973 on `Ismets-MacBook-Pro.local`.
   The live pointer could not be inspected safely. This was initially an
   **unheld-candidate** classification, not deletion authority.
4. Between 09:20 and 09:33 the live store independently removed all ten candidates
   and their manifests. Generation 27 and its reader survived. This establishes
   that the ten candidates were reclaimable at the daemon's later protection
   recheck; they were not references held by different checkouts. We did not
   trigger a cleanup command. Attribution to a particular retention invocation
   cannot be proved from file stats alone.
5. A reproducible leak explains the repeated generation-25 builds: an abandoned
   assembler's `Drop` tries to delete its files **before closing its own SQLite
   checkpoint keeper**. The file-identity guard refuses deletion, correctly.
   Only afterward does Rust drop the keeper field. Cancellation, callback errors
   after materialization and CAS losers therefore leave a full database plus
   manifest until a later sweep. The fix closes the keeper first and serializes
   cleanup with query-pin admission, preserving live readers.

The trailing 64-hex suffix is `AssemblyRequest.desired_head`, computed by
`head_tree_fingerprint` from tracked Git HEAD entries. It is **not a hash of the
SQLite bytes or even necessarily of the assembled working-tree manifest**.
Generation names are `counter-pid-time-serial-desired_head`. Publications
142/149/etc. are serials, not generation counters: all those names start with 25.
Repeated 25s are consistent with preparations from generation 24 that never won
CAS. Filesystem evidence alone cannot distinguish cancellation from CAS loss.

## Measured bytes

These are changing, nontransactional inventories. Logical bytes are `st_size`;
allocated bytes are `st_blocks * 512`. Allocated blocks are **not exclusive APFS
extents**, so they are not guaranteed reclaimable free space.

| Sample UTC | Domain | Logical bytes | Allocated bytes | Files | Top-level entries |
| --- | --- | ---: | ---: | ---: | ---: |
| 09:20:50 | views | 110,367,280,937 | 110,733,742,080 | 8,723 | 1,602 |
| 09:20:50 | blobs | 51,404,764,424 | 51,446,902,784 | 391 | 77 |
| 09:39:36 | views | 102,024,441,799 | 102,373,142,528 | 8,469 | 1,609 |
| 09:39:36 | blobs | 51,409,641,288 | 51,451,838,464 | 395 | 77 |

The views total fell while new checkout directories and artifacts were also being
created. It is not correct to equate the global delta to the selected folder's
reclaimed file sizes.

### Selected folder: derived database breakdown at 09:20

| Category | Logical bytes | Allocated bytes | Meaning |
| --- | ---: | ---: | --- |
| Referenced by live reader | 991,338,496 | 991,551,488 | Generation 27; pointer status initially unknown |
| Superseded / unpublished, unreferenced at later cleanup | 9,753,464,832 | 9,760,522,240 | Ten unheld candidates, all subsequently removed by the live store |
| Dead-checkout-referenced | 0 | 0 | This is an existing main checkout, and no dead-checkout pin/ref was found |
| Legacy unversioned, not classified as a generation | 266,330,112 | 266,518,528 | `derived.sqlite`, still present; do not delete by the generation rule |
| **All derived databases** | **11,011,133,440** | **11,018,592,256** | Sum of the disjoint rows above |
| Duplicate excess, identical manifest (**subset**, not additive) | 1,951,510,528 | 1,952,595,968 | Copies 149 and 159, keeping one representative per identical-manifest pair |

Duplicate means identical plain manifest bytes, **not proven identical SQLite
bytes**. All generation databases had different inode numbers and `st_nlink=1`.
The two identical-manifest groups were 142/149 and 158/159. Serial 153 shared
158/159's HEAD suffix but had a different manifest, so it is excluded from the
exact-manifest duplicate count. Byte equality does not itself prove compatible
build-output schema or readiness; reuse must also preserve those checks.

### Every selected derived file and its references

Names below abbreviate only the long HEAD suffix. The exact generation is
`counter-83973-nanoseconds-serial-suffix`; each database is
`derived-<generation>.sqlite`, with a corresponding `manifest-<generation>.json`.
All 11 had a zero-byte `trigram-<generation>.bin`. **None had a `.ref` or assembly
pin.** No query marker was present for the first ten; generation 27 had the sole
live marker. Manifests contain entries, not checkout ownership references.

| Counter / serial | Nanoseconds | HEAD suffix | DB logical bytes | DB allocated bytes | Manifest SHA-256 | Later state |
| --- | --- | --- | ---: | ---: | --- | --- |
| 24 / 113 | 1791412723474454000 | 910f7e95dfd5f1407edeacccb53a5d4ebe63ccd7ce83859475835897e423a2fa | 972,840,960 | 972,886,016 | b52580a8d38fe47c56318cbdc2068364f2524b0766223e48041b7c0ffc078391 | Removed |
| 25 / 142 | 1791417056388294000 | 143e5d7ebf02803ea9343e4d3495f7a0b70a653248ffdd07144c53dca7903a10 | 975,704,064 | 976,408,576 | c8eaded505d338949e644887585f71238c81ddf1e5d89b90e8466120e0ce1cba | Removed |
| 25 / 149 | 1791417308652994000 | 143e5d7ebf02803ea9343e4d3495f7a0b70a653248ffdd07144c53dca7903a10 | 975,704,064 | 976,683,008 | c8eaded505d338949e644887585f71238c81ddf1e5d89b90e8466120e0ce1cba | Removed |
| 25 / 151 | 1791417862192477000 | ccc55346cc4040652457bdd26e2c71cfc2f381a2d9bff91fb3735d35b470bee2 | 975,806,464 | 977,997,824 | 25fbb2eac80668289c01a001f7a42ec3fe0dd5c3c9405b77e4c05e727e1b064e | Removed |
| 25 / 153 | 1791418145778498000 | b16cfe95d26a03a035948e3720355f162b619c144a53ab5454c8bf48c56ef8eb | 975,802,368 | 976,486,400 | 7ff3aa83ee20dc68ee7c7bb590682f3fb0d9d50df273c97e0e55d864dd526705 | Removed |
| 25 / 158 | 1791418644162408000 | b16cfe95d26a03a035948e3720355f162b619c144a53ab5454c8bf48c56ef8eb | 975,806,464 | 976,879,616 | 5afa2397d49590e6bf1cd24eeb0bc204141b40dcd7c04dbfe5211c89f2341559 | Removed |
| 25 / 159 | 1791418725810372000 | b16cfe95d26a03a035948e3720355f162b619c144a53ab5454c8bf48c56ef8eb | 975,806,464 | 975,912,960 | 5afa2397d49590e6bf1cd24eeb0bc204141b40dcd7c04dbfe5211c89f2341559 | Removed |
| 25 / 164 | 1791419268660979000 | 7ce10d2e9f8911a404933c66fda613a2059092212aa33c520f88ad3a96d02fb2 | 975,818,752 | 975,876,096 | 1b22112f6dcdf636deab1daa5723a11629ca2403448efeedca83e69182c83b9d | Removed |
| 25 / 173 | 1791420151334888000 | c7e0398f128cb03c9f731422013e60adc759dafeb49ee88279fc335ab0ad4d8b | 974,528,512 | 975,331,328 | 89efaf339f1784a4c7da49fec50b0e3b0c67ef9db49983556cb37734ecaa226c | Removed |
| 26 / 257 | 1791424399074783000 | a742d590fc1edf1c5cb1f0c8131298a78d0d3b21a0df6e67ef1971d6b04e1d16 | 975,646,720 | 976,060,416 | 84aafd1dc6a1a54f3e13799c84be431eee545850856542fc2cfed81b426f7699 | Removed |
| 27 / 477 | 1791446464500066000 | 91f1b40dd0345ef2bf6251ebbe7234b3f0789533a1019786f0a33f022e5addfd | 991,338,496 | 991,551,488 | 75aa16be281b53d9a56fc33d1686076a2ee380e4d503ca647bc4730e9b674785 | Kept, live reader |

The reader is
`readers/27-83973-1791446464500066000-477-91f1b40dd0345ef2bf6251ebbe7234b3f0789533a1019786f0a33f022e5addfd/83973.Ismets-MacBook-Pro_local.1791447493879.16487.json`.
Its JSON reports PID 83973, the local hostname, and creation time 1791447493879 ms;
the PID existed. Older reader-generation directories were empty. At 09:33 the
entire selected folder was 1,258,933,049 logical / 1,259,339,776 allocated bytes.

### Global attribution and remaining gaps

At 09:33, joining canonical checkout path hashes from `cache-keys.json`, durable
bindings and owner manifests to top-level view directories gave:

| Checkout classification | View dirs | Generation DBs | Logical derived bytes | Allocated derived bytes |
| --- | ---: | ---: | ---: | ---: |
| At least one attributed root exists | 227 | 114 | 25,265,463,296 | 25,329,016,832 |
| All attributed roots absent | 1,297 | 285 | 70,293,934,080 | 70,531,100,672 |
| No attributed root | 84 | 7 | 2,811,408,384 | 2,818,314,240 |

Existing-root generation DBs with PID-live or conservatively protected foreign-host
markers accounted for **17,404,985,344 logical bytes**. Missing-root generation
DBs with such markers accounted for
**0 bytes** in that sample. Redundant identical-manifest copies within each view
had already fallen to **100,466,688 logical / 100,904,960 allocated bytes** (one
excess file). These are overlapping metrics, not additional categories.

A 09:39 census using **durable binding records only** found 224 missing-root
view directories, with **11,825,016,832 logical derived bytes**, all bound less
than seven days ago; none with a durable record was missing for seven days.
Another 1,214 dirs lacked that durable record (many still have older memo
attribution), with **64,459,689,984 logical derived bytes**. Missing durable
history is not evidence that deletion is safe: unknown keys get a fresh grace.

Two attributed roots existed but their `.git` files pointed to absent gitdirs:

- `.../alfonso/worktrees/e7c3d96c39df7bf6/bg_e4e37234134c08e2`
- `.../alfonso/worktrees/c0c39eea197fcc68/9e09c29b1299fe1375199f0556d08a60e3160822e6787a583c0054270883aec9/bg_2230b983de066dc3`

Neither had a `derived-*.sqlite` in its corresponding view directory. Retention
uses **root-path existence**, not Git registration; an existing folder with a
pruned/broken worktree is conservatively live. A deleted folder still has binding
history, which retains it for the missing-root grace and then becomes eligible
if every protection recheck permits deletion. Removing Git registration alone
is not sufficient. This is distinct from a live reader pinning a generation.

**Not measured:** pointer-backed current generations across the global store,
V2 registry contents, blob reachability and SQLite free pages. Reading those
would violate the no-live-SQLite constraint. Thus a complete global partition
into current versus unreferenced versus dead-checkout pointer references cannot
be honestly supplied from these observations. The selected folder's later
removal resolves that ambiguity for its ten candidates. Do not extrapolate its
ratio or treat the 70 GB of absent-root artifacts as an immediately safe purge.

## Retention and materialization mechanism

`ViewStore::sweep_generations_impl` in `views/generation.rs` takes a pointer write
transaction to serialize publication and ownership snapshots. It keeps:

- The durable current pointer generation.
- A generation with a live assembly pin (or unreadable/unparseable pin metadata).
- A generation with a protected query read marker; same-host live process
  instances do not expire merely because a marker is old.
- A derived owner named by **any** `.ref` in the ownership snapshot, even an
  obsolete reference until the next sweep. Its manifest is needed as an
  incremental-clone baseline; its unreferenced trigram can go.
- A derived file set with an in-process registered open SQLite connection.

The sweep does not keep every manifest, every previous generation, every Git
checkout publication, or a fixed count of same-hash copies. It serializes the
final protection check and unlink with pin admission. Root deletion additionally
requires missing and old bindings (seven days), mount identity checks, no
residency/read protection, writer leases, admission rechecks and no local open
connections. Unknown keys receive a seven-day observation grace.

Retention **does run after startup**, but not as an independent unconditional
timer. Configure maintenance and `subc/health.rs::build_health_diagnostic_rollup`
call `storage_retention::schedule`. The `HealthRollupWorker` runs diagnostic
rollups in its background loop with a three-second receive timeout (plus rollup
execution time), so health requests are not needed to drive this cadence. Scheduling is per-storage-root throttled to ten
minutes, after a five-minute startup grace, and requires a current bound
lifecycle context. Each pass examines at most 64 entries with a five-second
budget. The directory cursor continues between passes; cancellation resets it.
With roughly 1,600 view directories alone, one traversal requires at least 26
passes (over four hours at the minimum cadence), plus the other domains and
budget shortfalls. This is eventual cleanup, not ten-minute cleanup of every
folder. The delay makes abandoned-build leaks costly even when sweep safety is
correct. Configure also schedules a particular view's generation sweep.

`storage_retention::candidate` invokes `sweep_generations` for `views/<key>`
**before** deciding whether the checkout root may be deleted. Thus it covers
`views/<key>/derived-*` even for live roots. V2 family GC is a separate path.

A live checkout needs a compatible immutable **callgraph projection**, not a
new physical DB for every trigger. Existing safeguards already:

- Return the existing generation for a ready, unchanged HEAD and manifest.
- Publish `derived-<next>.ref` to the existing physical owner for a ready,
  callgraph-equivalent manifest (e.g. semantic fill). No database copy or rewrite.
- Otherwise clone through SQLite's backup API and patch, or cold-build. Shared
  content blobs are family-owned; V1 projections and `.ref` owners are scoped
  to one view. Identical content in different V1 checkouts can therefore have
  separate physical projections. Cross-checkout deduplication would require a
  shared owner/reference and retention protocol, schema/readiness identity and
  WAL/checkpoint handling. Hard-linking a mutable/WAL-mode SQLite file is not a
  safe shortcut. This report does not claim to implement that wider redesign.

The clear, narrow bug here is **unpublished retry files**, not retention keeping
same-hash publications from multiple checkouts. We preserve existing no-op,
reuse, schema-readiness and independent-view contracts. The patch removes the
assembler's own keeper before cleanup; it does not weaken file-identity guards
or retention grace. A live query pin postpones cleanup to a later sweep.

## Reproduction, verification and mutation proofs

Run tests on Linux only, with `[ "$(uname)" = Linux ] || exit 99` first. For each
run, create a unique `/tmp/aft-views-growth.XXXXXX` directory using `mktemp -d`,
then `mkdir -p` its `home`, `data`, `cache`, `config`, `state` children. Preserve
`CARGO_HOME`/`RUSTUP_HOME`, export HOME and the four XDG dirs to those children,
and `unset AFT_STORAGE_DIR` before invoking the worker-guide Cargo commands.
No test uses the live storage path.

`view_assembly_wiring_test::abandoned_derived_builds_release_keepers_before_cleanup`
failed on the base implementation with `abandoned file remains: derived-2-...sqlite`.
The test prepares real materialization, verifies durable files exist, drops it,
and asserts the DB/WAL/SHM/manifest/trigram are absent **without sweeping**. It
also exercises two identical concurrent preparations and a CAS loser, then a
callback error after durable preparation. The winner's pointer and DB survive.
`abandoned_derived_build_keeps_a_live_query_pin` ensures cleanup leaves files
while a query pin is live and a later sweep removes them after the pin drops.
The existing unchanged-republish, semantic-fill sharing and crash tests stay.

Mutation controls staged the live files, captured empty unstaged diff, introduced
`NON-VACUITY BREAK`, captured nonempty diff, ran the named check, restored with
`git checkout -- <path> && touch <path>`, and captured empty unstaged diff:

- Remove the early keeper close: only
  `abandoned_derived_builds_release_keepers_before_cleanup` failed (one pass,
  one failure); the live-pin test stayed green.
- Replace the live-query protection predicate with an always-true predicate for
  valid generation names: the exact
  `abandoned_derived_build_keeps_a_live_query_pin` test failed on missing DB
  (one failure, no other tests selected).

One earlier live-pin control attempt returned `remote outcome_unknown; never
rerun`, with no test result. Its mutant was restored immediately and its output
is not counted as evidence. A distinct exact-test control supplied the safety
proof. No mutant remains. Final Linux gates on rustc 1.99.0 / cargo 1.99.0:

- `cargo fmt --all -- --check`: exit 0 (rustfmt 1.10.0-stable).
- `cargo check -p agent-file-tools --tests`: `Finished dev`, passed.
- `cargo test -p agent-file-tools --test integration -- view_assembly_wiring_test`:
  9 passed, 1 existing full-checkout profiling test ignored.
- `cargo test -p agent-file-tools --lib -- executor::view_publication::tests`:
  15 passed.
- `cargo test -p agent-file-tools --lib -- storage_retention`: 25 passed.

The worker-guide Windows compile gate was attempted but failed in the dependency
`ring` because the remote environment lacks `x86_64-w64-mingw32-gcc`. It did not
reach AFT and is not claimed passed. Scoped `aft_inspect` remained partial while
rust-analyzer indexed; native package checking supplies the authoritative Rust
diagnostics. No TypeScript, package manifests, config keys or tool schemas changed.
The descriptor was validated with the benchmark library against all four changed
paths (zero fence hits). The offline cleanup example was syntax-checked and
exercised against throwaway files and a throwaway pointer DB: five checks passed
(dry-run preservation, apply preserving current/ref owner/legacy, live-reader
refusal, live-assembly-pin refusal, inconclusive-lsof refusal). The lsof responses in those example checks
were mocked; this is not evidence of live-storage quiescence.

The descriptor `benchmarks/aft-search/slice-descriptors/views-storage-growth-2026-10.json`
is **non_ranking**, derived from the actual changed paths against
`RANKING_FENCE_PREFIXES` in `search_quality_lib.py`: there are no fence hits.

## Exact safe manual cleanup (operator only; not executed)

**For this selected folder, the ten candidates were already gone at 09:33.**
There is no remaining 9.75 GB generation cleanup to run there at this sample.
Do not delete generation 27, `derived.sqlite`, arbitrary missing-root view dirs,
or blobs based on this report. Installing the fix prevents repeated abandoned
files; normal retention eventually sweeps already-abandoned files.

If candidates accumulate again and the disk cannot wait for the rotating sweep,
the following is a deliberately **offline**, selected-view-only procedure.
It cannot be made safe while the daemon, another AFT reader, or a supervisor
that can restart them is running.

1. Stop agent clients, the AFT daemon and all AFT modules through the operator's
   normal shutdown mechanism; disable their auto-restart for the entire operation.
   Merely sending SIGSTOP is **not** enough: paused processes still hold pins and
   open connections. Confirm no other host uses this storage.
2. With clients still stopped, save the following as a temporary operator script
   and run it with `python3`. It fails on open descriptors, live/foreign/unknown
   reader or assembler ownership, malformed metadata, invalid names and any
   SQLite error. It opens the pointer **only after quiescence**, never live.
   All `.ref` sources and owners are conservatively kept. It excludes the legacy
   unversioned database and never removes directories or blob stores.
3. Review the printed candidate list in its default dry run. Only then run the
   same script with `AFT_OFFLINE_CLEANUP_APPLY=yes`; keep supervisors disabled
   until it exits. Restart clients afterward. If any prerequisite is uncertain,
   do not use the script; let managed retention perform its locked rechecks.

```python
import json, os, pathlib, re, socket, sqlite3, subprocess, sys

store = pathlib.Path.home() / '.local/share/cortexkit/aft'
view = store / 'views/c0c39eea197fcc68'
# This is an additional check, not a replacement for stopping auto-restart.
probe = subprocess.run(['lsof', '-nP', '+D', str(store)],
                       capture_output=True, text=True)
if probe.returncode != 1 or probe.stdout or probe.stderr:
    sys.exit('Refusing: open descriptors or inconclusive lsof census')
if view.is_symlink() or not view.is_dir():
    sys.exit('Refusing: unexpected view directory')

def generation(value):
    if not isinstance(value, str) or not re.fullmatch(r'[A-Za-z0-9_-]+', value):
        raise RuntimeError('invalid generation')
    return value

# Strictly fail closed; even a possibly reused live PID prohibits offline cleanup.
for path in list((view / 'pins').glob('*.json')) + list((view / 'readers').glob('*/*')):
    record = json.loads(path.read_text())
    if path.parent == view / 'pins':
        # Assembly PinOwner has pid/start_time, no hostname. The offline
        # prerequisite excludes other hosts, and any live local PID still refuses.
        generation(record['generation'])
        owner = record['owner']
        if not isinstance(owner['start_time'], int) or owner['start_time'] < 0:
            sys.exit('Refusing: invalid assembly process identity')
    else:
        owner = record
        if owner['hostname'] != socket.gethostname():
            sys.exit('Refusing: foreign reader')
    pid = owner['pid']
    if not isinstance(pid, int) or pid <= 0:
        sys.exit('Refusing: invalid owner PID')
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        pass
    else:
        sys.exit('Refusing: live owner')

# Read-only, not immutable: do not ignore any remaining committed pointer WAL.
connection = sqlite3.connect(view.joinpath('pointer.sqlite').as_uri() + '?mode=ro',
                             uri=True)
rows = connection.execute('SELECT generation FROM pointer WHERE singleton=1').fetchall()
connection.close()
if len(rows) != 1 or not rows[0][0]:
    sys.exit('Refusing: missing current pointer')
keep = {generation(rows[0][0])}
for path in view.glob('derived-*.ref'):
    keep.add(generation(path.name[8:-4]))
    keep.add(generation(path.read_text()))

candidates = set()
for pattern, prefix, suffix in [('derived-*.sqlite', 'derived-', '.sqlite'),
                                ('manifest-*.json', 'manifest-', '.json'),
                                ('trigram-*.bin', 'trigram-', '.bin')]:
    for path in view.glob(pattern):
        candidates.add(generation(path.name[len(prefix):-len(suffix)]))
paths = []
for name in sorted(candidates - keep):
    for base in [f'derived-{name}.sqlite', f'manifest-{name}.json', f'trigram-{name}.bin']:
        for suffix in ['', '-wal', '-shm']:
            path = view / (base + suffix)
            if path.is_symlink():
                sys.exit('Refusing: symlink artifact')
            if path.exists():
                if not path.is_file():
                    sys.exit('Refusing: non-file artifact')
                paths.append(path)
for path in paths:
    print(path.name, path.stat().st_size)
if os.environ.get('AFT_OFFLINE_CLEANUP_APPLY') == 'yes':
    for path in paths:
        path.unlink()
    print('Removed', len(paths), 'offline unreferenced files')
else:
    print('Dry run only; keep:', sorted(keep))
```

This script is not a replacement for GC and cannot justify a global store purge.
It deliberately preserves orphan `.ref` generations for the managed two-pass
ownership sweep, and does not delete stale pin/reader records. A live-mode
`rm derived-*`, SQLite hard-link replacement, or a read-only SQLite probe of the
production store is not an acceptable cleanup procedure.

## Follow-up: root grace survives daemon replacement

Follow-up base: accepted commit `95bd24da8bf3081043ef921aaf2aa6b7b134e5aa`.

### 1. The clock is already persisted; the suspected restart reset is absent

There are **two different clocks**, and neither depends on process-local
first-sighting memory:

- **Attributed key:** `eligible` loads binding history and requires **every**
  recorded root to be absent and every binding to pass `missing_and_old`.
  That predicate ages `last_bound_ms`, not the time deletion was observed
  ([storage_retention.rs:259–275](../../crates/aft/src/storage_retention.rs#L259-L275),
  [587–607](../../crates/aft/src/storage_retention.rs#L587-L607)). The effective
  age gate is seven days after the **latest** binding for the key, plus all
  mount/protection checks. A root last bound eight days ago can become eligible
  immediately when it disappears; there is not an additional seven days after
  observing its disappearance.
- **Unattributed key:** the first call to `eligible` that reaches the no-history
  branch creates `<storage>/retention/unknown/<key>.json`, containing the
  first-observed Unix time in milliseconds. Subsequent calls **read that file**;
  only `NotFound` creates a new clock. Malformed/unreadable metadata returns an
  error rather than restarting the clock
  ([storage_retention.rs:608–626](../../crates/aft/src/storage_retention.rs#L608-L626)).
  This clock is shared by that key across cache domains; it is not necessarily
  the first time its *view* directory was visited, or a known root's deletion
  time. The file is created with temporary-file write plus atomic rename
  ([161–172](../../crates/aft/src/storage_retention.rs#L161-L172)). Ordinary daemon
  exit/replacement does not erase it. This is atomic replacement, not a claim of
  fsync-based power-loss durability; losing the file would restart a conservative
  retention delay, not authorize early deletion.

**No `retention/roots` record does not mean no binding history.** `bindings`
loads `cache-keys.json` records as well as durable root files and owner manifests
([348–428](../../crates/aft/src/storage_retention.rs#L348-L428)). The earlier
1,214-directory count was *durable-record absence*, not 1,214 keys using the
unknown-key grace. Most still have memo attribution. The seven-day constant is
`ROOT_GRACE_MS = 7 * 24 * 60 * 60 * 1000` = **604,800,000 ms**
([line 18](../../crates/aft/src/storage_retention.rs#L18)); it is a hard-coded
retention policy, not a user-configurable setting. It remains unchanged.
`record_bind` records a new durable `last_bound_ms` on every configure, including
a same-key rebind ([174–191](../../crates/aft/src/storage_retention.rs#L174-L191)).

What *does* reset on daemon replacement is scheduler state: startup warm-up,
last-run throttle and the directory traversal cursor are in `State`/`states`
([66–92](../../crates/aft/src/storage_retention.rs#L66-L92)). Resetting the cursor
can delay discovery/revisiting of a tail directory, particularly with repeated
short-lived daemons. It does **not** reset the persisted observation once found.
The fresh five-minute startup delay also remains. No new persistence mechanism,
retention-policy change, or production logic fix was needed for this hypothesis.

### 2. Missing-root bytes by the clocks that actually exist

Plain-metadata/stat census at **2026-10-08T10:04:12Z**, with age calculations using
`now_ms = 1791453850706`. This follows the previous samples, so changing directory
counts are expected. Only top-level V1 `views/<16-hex-key>/derived-*.sqlite` main
files are counted here; legacy `derived.sqlite`, V2, sidecars, manifests and blobs
are excluded. Allocated sizes retain the APFS shared-extent caveat above.
Attribution joins the actual history sources: memo root-scope and artifact keys,
durable root-file keys and artifact keys, and owner scope/artifact keys. No live
SQLite was opened and no metadata was written.

| History classification | View dirs | Derived DBs | Logical bytes | Allocated bytes |
| --- | ---: | ---: | ---: | ---: |
| Attributed, all roots absent | 1,314 | 290 | 70,562,852,864 | 70,800,789,504 |
| Attributed, at least one root present | 224 | 114 | 28,501,434,368 | 28,587,876,352 |
| No history attribution | 83 | 7 | 2,811,408,384 | 2,818,314,240 |

**Missing attributed roots — latest last-bind age, not missing-since age:**

| Age bucket | View dirs | Derived DBs | Logical bytes | Allocated bytes |
| --- | ---: | ---: | ---: | ---: |
| [0, 1) days | 359 | 71 | 20,711,718,912 | 20,750,151,680 |
| [1, 2) days | 222 | 72 | 16,232,099,840 | 16,320,778,240 |
| [2, 3) days | 237 | 52 | 7,964,172,288 | 8,004,476,928 |
| [3, 7) days | 496 | 95 | 25,654,861,824 | 25,725,382,656 |
| [7, 14) days | 0 | 0 | 0 | 0 |
| [14, 30) days | 0 | 0 | 0 | 0 |
| >=30 days | 0 | 0 | 0 | 0 |

All **70,562,852,864 logical bytes** in the missing-root category were attributed
only to worker-worktree paths containing `/cortexkit/alfonso/worktrees/`: 1,300
view dirs / 290 DBs. The other 14 missing-root dirs had zero generation-DB bytes.
The worker-only directory counts in the four nonempty buckets were 357, 222,
237 and 484 respectively; the byte totals were identical to the table. Thus
**44,907,991,040 bytes** were last bound less than three days ago, and
**25,654,861,824 bytes** were last bound three to seven days ago. No attributed
missing root in this sample had passed the seven-day *age* gate. These numbers
explain a large young-worktree backlog without any restart-reset bug.

**Active unknown-key clocks — persisted first-observation age:**

| Observation age | View dirs | Derived DBs | Logical bytes | Allocated bytes |
| --- | ---: | ---: | ---: | ---: |
| [0, 1) days | 75 | 6 | 2,409,709,568 | 2,415,595,520 |
| [1, 2) days | 0 | 0 | 0 | 0 |
| [2, 3) days | 0 | 0 | 0 | 0 |
| [3, 7) days | 0 | 0 | 0 | 0 |
| >=7 days | 0 | 0 | 0 | 0 |
| No observation file yet | 8 | 1 | 401,698,816 | 402,718,720 |

The entire `retention/unknown` directory contained **1,792 valid timestamp JSON
files**, zero malformed files. Its oldest timestamp was `1791405304627`
(2026-10-07T20:35:04.627Z), newest `1791453389995`
(2026-10-08T09:56:29.995Z). Many timestamps concern other cache domains or keys
whose view directory is absent, so 1,792 is not a count of unknown view dirs.
For example, `retention/unknown/001084a169624dce.json` contained
`1791446780041`; its key had no binding-history attribution. There was no
unknown-clock file for the live prefrontal scope `c0c39eea197fcc68`, which has
memo, durable and owner history.

There is **no persisted first-observed-missing timestamp for attributed roots**.
The true duration since their folders disappeared cannot be recovered from
last-bind timestamps or current filesystem metadata. The last-bind and unknown
observation tables above deliberately do not label either as actual deletion
age. Unattributed keys also cannot be assigned to worker paths safely.

### 3. Do permanently settled worker worktrees need seven days?

Given the operator's guarantee that settlement permanently deletes a worker
checkout, it needs **no durability grace for recovering that checkout**. The
seven days are a conservative cache-retention policy, not a requirement of an
immutable derived DB or a live reader. The implementation already treats legacy
worker-path bindings specially: absent-root bindings with no mount identity are
eligible after the age gate **only** for paths containing
`/cortexkit/alfonso/worktrees/`; unidentifiable other mounts remain protected
([259–275](../../crates/aft/src/storage_retention.rs#L259-L275)). It does not give
workers a shorter age gate, nor store authoritative settlement status.

A future shorter/zero worker-specific grace would still need all existing
reader/pin, writer-lease, residency, admission and open-connection checks
([candidate](../../crates/aft/src/storage_retention.rs#L626-L800)); permanent
folder deletion is not proof that no reader remains. Unknown keys cannot receive
a worker exemption without reliable attribution. An authoritative settled-worktree
signal would be stronger than guessing from a missing folder or `.git` pruning.
No grace value or deletion rule is changed in this follow-up. The measured
70.56 GB is the byte population for the operator's policy decision, not an
immediate-purge authorization.

### 4. Actual restart regression and in-memory mutation red

Added
`storage_retention::storage_retention_tests::storage_retention_unknown_observation_survives_process_restart`.
It creates throwaway view storage and an old payload, then invokes retention's
`candidate` in **three fresh OS test processes**, with supplied times:
first observation, one millisecond before the seven-day boundary, and the exact
boundary. It requires preservation before grace and actual directory reclamation
at the boundary, and checks after **every** process that the original timestamp
file is unchanged. It never rewrites/backdates that observation file. The test
advances the retention function's existing `now` argument; it does not sleep or
change the production clock. The fixture has no pointer DB, keeping the test
focused on root observation grace rather than generation GC.

The already-persisted implementation passed the new test before any runtime
change (one test passed); there was no failing production case to pretend to fix.
For non-vacuity, a `NON-VACUITY BREAK` shadowed the file-loaded clock with a
process-local `OnceLock<Mutex<HashMap<PathBuf, u64>>>` initialized on each
process's first observation. The timestamp file still existed with its original
value, so the test could not pass merely by checking that file. Only the named
restart regression ran and failed at the actual reclamation assertion:
`removed_roots` was **0**, expected **1**. All child invocations completed
successfully. The control's diff was 11 inserted lines; it was restored from the
staged live state with `git checkout -- ... && touch ...`, leaving an empty
unstaged diff. This is evidence against a genuinely in-memory grace clock, not
just a timestamp-file existence test.

The follow-up changes only an explanatory comment, regression tests, this report
and the existing non-ranking descriptor. The accepted drop-order fix remains
unchanged. No live cleanup, timestamp update or live SQLite probe was performed.

Follow-up verification (Linux rustc/cargo 1.99.0, rustfmt 1.10.0-stable):
`cargo fmt --all -- --check` passed; `cargo check -p agent-file-tools --tests`
finished successfully; `cargo test -p agent-file-tools --lib -- storage_retention`
passed **27 tests**, including the real fresh-process regression. Runs used
throwaway HOME and XDG directories outside checkouts, `AFT_STORAGE_DIR` unset,
and the Linux uname guard. Scoped `aft_inspect` was partial while rust-analyzer
ran its check/build scripts; the package check is authoritative. Windows checking
remains unavailable in this remote environment because the MinGW compiler was
missing in the preceding verification; no Windows pass is claimed. The follow-up
path diff has no ranking-fence matches and retains the non-ranking descriptor.
