# Watcher delivery-frontier audit

## Decision and scope

This audit concerns **verified bind**, not whether ordinary watching works. A
cookie acknowledgment must mean that every relevant write completed before
cookie creation has reached the views driver as reflected data or pending work.
An empty queue, a quiet interval, or a native flush returning is not that proof.
The cookie lives directly in the checkout root and is created exclusively as
`.aft-cookie-<pid>-<nonce>`; it must never become corpus membership.

The audit starts from commit `a8e301bea1eb7d2cba7ecbaccfeecc4a461c6138`.
No backend is enabled for verified bind by this slice. In particular, FSEvents
is **off pending proof**, not demonstrated incorrect. Cookie/overflow
infrastructure can be built without relaxing that restriction. E0 is the start
of a fully registered watcher subscription; F is the later cookie delivery
frontier. A delivery sequence orders invalidations at the driver, and the
durable cut is the exact prefix of those changes persisted in a publication.
Connecting those points to ready publication belongs to later verified-bind work.
Ordinary strict reconciliation remains available with its existing, weaker
per-path stability and subsequent-event-invalidation contract.

## Shared filters, batching and the driver queue

`watcher_filter::WatcherFilterThread` receives normalized native events over an
MPSC channel. Ordinary paths are canonicalized, deduplicated, and batched before
corpus filtering. `send_dispatch` and `retry_backlog` feed a bounded FIFO driver
channel; held batches may merge, and overflow replaces paths with a rescan.
Consequently, observing a cookie at raw intake alone cannot acknowledge a
views frontier: earlier paths may still be in either batch, backlog, channel,
or `runtime_drain`'s sliced `pending_paths`.

The additive cookie plumbing must bypass ignore/infra filters, unlink a root
cookie immediately at intake, flush earlier paths, and enqueue a distinct
control rather than a membership path. A mixed event must retain its non-cookie
paths. Only a create event may acknowledge a ticket; unlink/metadata events must
not. The shared `watcher_backend::flush::is_marker_path` admission predicate must
also reject cookies during a filesystem walk, not just during event delivery.
The command-report `.aft-flush` acknowledgment remains a separate protocol.

The driver-side cookie arm must record all preceding queued paths as pending
before acknowledging the exact registered cookie. A rescan must enter the
driver as `WatcherState::Overflowed` and invalidate an in-flight installation;
a subsequent ordinary path must not reset that state to Healthy. A cookie held
in a backlog whose paths are discarded must be discarded too (timeout, not a
false successful frontier). This provides an ordered **delivery acknowledgment**,
not ready publication: the bind owner must still establish E0, reject any gap,
replay through the acknowledged cut, and revalidate before publication.

The planned additive implementation includes regression tests named
`watcher_cookie_delayed_delivery_past_read_inotify`,
`watcher_cookie_delayed_delivery_past_read_fsevents`, and
`watcher_cookie_delayed_delivery_past_read_windows`, using a backend-delivery shim
which holds a write notification and its following cookie past a source read.
They must show no acknowledgment before release and a pending path before the
acknowledgment afterwards. They test the application queue, **not** a kernel's
completed-write coverage. Native macOS and Windows execution is delegated to CI.

## Linux: inotify

`inotify::ProjectWatcher::create_with_plan` creates one notify inotify instance,
collects directories, and synchronously installs non-recursive watches before
spawning its backend thread. Every per-directory descriptor belongs to that
instance's ordered queue; extra watches also use that instance. Successful
return is later than registration of the collected set, but is not yet a safe
E0: `collect_watch_directories` walks **before** the root watch is installed.
A directory created after its parent was enumerated but before the parent was
watched can be absent from the collected set without a queued create event.
The later bind must close that registration gap before recording E0, and must
not mistake a returned watcher handle for a complete subscription proof.

For newly delivered directory creates/renames, `watch_new_tree` does install
each directory's watch before `exclusion_read_dir`. Already-populated files are
recovered by listing. `PendingNewDirectoryFiles` delays that synthetic file
batch; it must be flushed before a root cookie even when command-report tracking
is inactive. Otherwise the root cookie could overtake a recovered child write.
An independent metadata walk cannot assume the backend has already processed
every directory create: watch-before-list coordination with that walk remains
an enablement obligation of the bind implementation.

Upstream notify's inotify queue-overflow flag is forwarded as `need_rescan`.
An incomplete/over-budget new-tree listing also emits a rescan. The filter maps
these to `RescanRequired`, and the context adapter must mark the installed
driver Overflowed before any subsequent cookie acknowledgment. Watch-limit and
other fatal backend errors terminate delivery; a disconnected/error stream
cannot certify a frontier. Retained watches are re-added without unwatching in
`reconcile_watch_directories`, but policy/boundary changes still require bind
revalidation. A single queue is necessary, not proof that every directory was
covered when a write occurred.

## macOS: FSEvents

`fsevents::FsEventsStream::start_paths` starts one recursive project stream with
FileEvents and waits for `FSEventStreamStart` on its run loop. There is no
per-directory watch-registration walk: a recursive stream covers newly created
descendants without adding directory handles. Excluded/cross-device descendants
are not thereby certified. A later bind can record E0 only after successful
startup and coverage/root/policy validation, not before the startup handshake.

The callback translates in native array order, hands off one vector, and advances
its replay cursor only after handoff. The backend forwards each vector in order
to the filter. Stream replacement replays from the old delivered event ID rather
than SinceNow; a missing/wrapped cursor requires a rescan. MustScanSubDirs,
UserDropped and KernelDropped all emit `Flag::Rescan` in `translate_event`;
those controls must survive filters and mark the driver Overflowed.

FSEvents is **off: completed-write-before-cookie coverage through the views
queue is unproved**. Auxiliary streams are independently ordered
(`fsevents.rs:163-164` at the audited commit). FlushSync covers only events
already in the pipeline (`fsevents.rs:214-222`), not all completed filesystem
writes. Preserving callback/event-ID order does not by itself close that gap.
No root cookie certifies an independently ordered auxiliary stream. No
quiet-window substitute is accepted.

The later native test required to turn it on is
`fsevents_completed_writes_before_cookie_reach_views_frontier`: on a writable
same-device local root, establish E0, hold delivery beyond the stat/content read,
complete writes in multiple existing and newly created descendants, create the
cookie, and prove every pre-cookie write reflected or pending through the exact
views delivery cut before ready publication. Exercise coalesced callbacks,
stream handoff, and drops (drops must refuse verified bind); separately fence
auxiliary streams or exclude their state from the frontier claim. The shim test
above is necessary application-ordering evidence, not this native proof.

## Windows: ReadDirectoryChangesW

`windows::ProjectWatcher::start` blocks on `start_rx` until `open_watches` has
opened and submitted every initial read. `project_watch_targets` includes one
recursive root handle and separate non-recursive extra-path/parent handles.
Each `DirectoryWatch` has its own ordered notification buffer and pending read.
The recursive root handle already covers newly created directories; there is no
per-child registration/listing seam within that handle. E0 must follow all read
submissions and root/target validation.

`run_completion_loop` rearms a read before translating its completed buffer, and
forwards records in buffer order. A completion port merges independent handles;
it does not give their writes a shared filesystem ordering. A root cookie
certifies neither an earlier undelivered extra-handle completion nor handle
replacement. Single-recursive-handle checkout-only coverage is a possible later
restriction, not a proof supplied by this slice.

ERROR_NOTIFY_ENUM_DIR and successful zero-byte completions produce the typed
buffer-overflow rescan. These must mark the driver Overflowed; other fatal
completion errors end the stream and invalidate any bind. DirectoryWatcher
replacement drops its old watcher before starting the next; no verified bind
may span that registration gap. Native completed-write coverage (including
last-write/close semantics) and multi-handle fencing remain unproved.

## Verified-bind on/off table

These are enablement decisions, not changes to ordinary watcher selection.

| Backend / root | Verified bind | Reason / remaining obligation |
| --- | --- | --- |
| Linux inotify, writable local same-device root | **Off pending coverage proof** | Initial collect-before-watch gap; coordinate new-directory registration before subscription E0 and during the bind walk through cookie frontier F. One instance plus ordered cookies alone is insufficient. |
| macOS FSEvents, writable local same-device root | **Off pending proof** | Completed-write-before-cookie coverage through views queue unproved; independent auxiliary streams; FlushSync only covers events already in pipeline. Native turn-on test named above belongs to later verified-bind work. |
| Windows native recursive root plus auxiliary handles | **Off pending proof** | Ordered buffers are per handle, not a global frontier; native completed-write coverage and handle lifecycle must be proved. |
| notify fallback; polling; any other unproved backend | **Off** | No proved ordered completed-write frontier. |
| Network filesystems (including NFS/SMB); FAT | **Off** | Backend/local timestamp and delivery assumptions do not apply. |
| Read-only roots | **Off** | Cannot create an exclusive cookie. |
| Cross-mount or cross-device subtrees | **Off for the subtree** | A root stream/cookie does not prove coverage on another mount/device. |
| Cookie timeout, watcher loss, root/policy change, unregistered directory | **Off for that bind** | Strict reconcile; never treat silence as continuity. |
