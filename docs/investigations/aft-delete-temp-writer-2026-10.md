# External temp delete and root bind starvation (2026-10)

## What the code and reproduction establish

The incident ran on `1821346c895af444ce2a761b4955ae69c027a782`. The logged
`exec=334964ms` covers the entire handler, not an individual filesystem call.
The original tree is gone, and the log has neither its entry count nor a
syscall trace. It is not possible to attribute all 335 seconds to one syscall
or to prove an ENOSPC stall retrospectively.

The system-temp fast path is real: `whole_tree_skip_reason` selects TempPath
before `walk_tree`, backup budgeting, snapshots or backup metadata syncing.
It performs one `read_dir`/`file_type` preflight per descendant, checks device
identity only for directories, then calls the opaque, uncancellable
`std::fs::remove_dir_all` (another traversal). After removal it calls
`lsp_notify_watched_config_file` for **every leaf**, even outside the project.
That does TS-selection checks/global invalidations, custom marker/config
lookups and, for config filenames, LSP event path resolution/locking. There
are no delete retry loops, sleep/backoff or explicit fsyncs in this path;
unsupported-symlink/special-file and backup byte-budget probes are bypassed.
The AFT preflight is linear, not quadratic. The uninstrumented standard
library and filesystem can still stall, especially under disk pressure.

Reproduction on this macOS worker: 30,000 seven-byte files, 100 directories,
one root in the system temp directory, with an unrelated project root.
No production files or disk-full simulation were used. The library fixture
disables backups explicitly because library tests normally bypass the temp
policy; integration safety tests exercise the production temp policy.
Timing excludes fixture creation. These are single observations, not stable
wall-clock performance promises:

| Handler | Entries | Seconds | Entries/second | Counted AFT operations |
| --- | ---: | ---: | ---: | --- |
| Before | 30,101 | 2.848 | 10,569 | 30,100 preflight visits; 30,000 per-leaf LSP calls; then opaque `remove_dir_all` |
| After | 30,101 | 1.914 | 15,729 | 30,100 preflight visits; 30,101 descriptor-relative unlink attempts; zero per-leaf LSP calls |

A later concurrent 77-test run took 31.246 seconds (963 entries/second) for
the same optimized delete, without changing any operation counts. Disk/load
variance is large on this shared worker; these samples must not be read as an
explanation of the live daemon's full duration or a guaranteed speedup.

The full 335-second duration was **not reproduced**. The reproducible defects
are needless second enumeration/per-leaf LSP work and unbounded execution
after abandonment, coupled to a project-wide writer barrier. Operation-count
tests, rather than a timing threshold, protect the improvement. Counters are
at the actual preflight, notification and unlink call sites, not inferred
from the response or compared to themselves. On Unix unbacked leaves need no
stat/readlink/content probe: `unlinkat` does not follow links and rejects a
directory substituted for a leaf. Directories are opened no-follow, and their
device is rechecked before descent. New entries after preflight are left
alone: an unempty directory stops removal, with partial progress reported.

## Why a bind can proceed safely

The original manifest put every delete in `Lane::Mutating`. That reserves the
actor writer barrier and epoch write gate for the whole handler. Root binds
reconcile configuration through that same gate, so timeout of the caller
does not free the root while its job continues running.

Delete now uses the existing deferred-response seam. Its short Mutating
setup validates every target under the admitted restriction/config, resolves
parent paths, and classifies the whole batch. Only **unbacked, wholly external**
targets are detached. A target inside the root, an ancestor containing the
root, a symlinked parent resolving inside it, an unknown identity, a mixed
batch, or a target requiring snapshots keeps the ordinary writer barrier.
Inside-root deletes therefore still exclude binds, readers and other writers.

The external continuation pins the admitted config and cancellation token,
and preserves the admitted no-backup decision even if another bind changes
backup policy. It never writes project indexes/config or undo snapshots and
does not send workspace LSP notifications for external paths. The only shared
backup update is the skipped-operation record, protected by the backup store's
own mutex. External backed-up deletes remain writers conservatively: their
undo state belongs to the actor. This changes no generic bind admission rule
and weakens no inside-root writer guarantee.

## Cancellation and partial results

Delete does not seal an atomic commit: each unlink is an incremental mutation.
Explicit `JobCancellation` is checked before every removal and preflight
entry. Root/lifecycle abandonment, which probes disk, is checked at most every
64 entries. Backed-up walks and snapshot loops check cancellation too. The
standard-library recursive remover is no longer used. A syscall already in
progress cannot be bounded or forcibly interrupted by cooperative checks.

The subc deferred seam uses CancelOnDetach, so transport loss no longer keeps
delete running for replay. An explicit Cancel retains the delete's completion
path instead of replacing it with a generic error; its terminal can therefore
report `request_cancelled`, removed file/directory counts, `remaining_root`
and `stopped_at`. `remaining_entries` counts preflight-recorded entries not
removed by this call (including the root); concurrent additions/removals can
make the current disk count differ. Cancellation during preflight reports no
deletes and an unknown remaining count, without doing another unbounded scan.
Batch skipped targets preserve these details. Partial unbacked stops are also
logged with progress and root location for callers that have disconnected.
For backed-up partial deletes, snapshots of removed entries are retained and
snapshots for intact entries are discarded, preserving undo semantics.

## Regression controls

- `large_temp_delete_has_linear_operations`: 30,000 leaves, one visit and one
  unlink per entry, no external per-leaf LSP work.
- `bind_beside_outside_root_delete_proceeds`: pauses a real external deletion;
  same-root Mutating bind completes before the delete is released.
- `bind_beside_inside_root_delete_waits`: same rendezvous inside the root;
  bind must wait until removal completes.
- `cancelled_delete_stops_between_entries_and_reports_remaining`: signals
  Cancel after seven unlinks and verifies 193 actual remaining files, the root,
  and the detailed terminal.
- `cancelled_external_delete_keeps_its_partial_terminal`: exercises active-call
  and deferred-response cancellation tracking through an actual external job.
- Admission also covers ancestors, mixed batches and symlinked parents.

Mutation runs and exact gate outcomes are recorded in the delivery declaration.
