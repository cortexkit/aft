# V1 blob GC: proposed admission-epoch handoff

Status: proposal; not implemented. V1 currently holds `retention/sweep.lock`
through marking and deletion. That exclusion is necessary for safety, but a
large family can delay query-pin admission and therefore view loads. Startup
maintenance is deferred to avoid competing with the initial checkout warm-up;
this proposal addresses steady-state contention separately.

## Invariant

A blob named by a published manifest or a live assembly/query pin must not be
deleted. Marking must not miss a pin admitted after its snapshot, including an
assembly that starts using a previously unreferenced blob. Crash recovery must
fail closed rather than interpreting missing or unreadable state as no pins.

## Proposed protocol

1. Persist a monotonic admission epoch for V1 storage. Every admission that can
   add references (bind, assembly pin, query pin, and publication if not already
   covered by an assembly pin) changes it while holding the existing barrier.
2. The collector captures the epoch, marks manifests and pins without holding
   the admission barrier, then captures candidate blob identities. Enumeration
   bounds or unreadable protection abort the deletion batch.
3. Before deleting a bounded batch, the collector acquires the barrier and
   rechecks the epoch. If it changed, the snapshot is stale: release the barrier
   and defer/re-mark, with no deletion. If unchanged, delete the batch under the
   barrier. The batch must have an explicit work bound; vacuum runs outside it.
4. Admissions serialize with that deletion batch. An admission after deletion
   must validate that its generation and required blobs still exist before
   succeeding. Admission cannot simply create a marker for a removed generation.

## Crash and restart cases requiring review

- An epoch write must be durable before the admission becomes observable. A
  crash between epoch publication and pin publication only causes a conservative
  collector restart. The opposite ordering is unsafe.
- Atomic replacement, concurrent processes, counter overflow, and corruption
  need explicit handling. Missing epoch state is not implicitly epoch zero when
  existing stores/pins may predate it.
- A collector crash after some deletes is recoverable only because every delete
  was justified under the unchanged epoch/barrier. The next pass marks again.
- A process restart must not reset the durable epoch or reuse an earlier value.
  Old binaries do not bump an epoch; mixed-version use needs a compatibility
  fence or the old barrier-held protocol, not optimistic unlocked marking.
- Pin removal does not require invalidating a conservative snapshot, but stale
  pins and read markers must be reclaimed without accidentally adding new
  references outside the epoch protocol.

## Why V2's epoch cannot just be reused

V2 admissions are owned by a family registry, with membership and protection
rows coordinated by its barrier and reference epoch. V1 manifests, assembly
pins, and reader markers are filesystem records across checkout directories;
V1 has neither registry membership nor that transactional handoff. Sharing an
API name would not make those filesystem references part of V2's transaction.
The V1 epoch must cover every reference-adding path and mixed-version access.

## Required adversarial verification

Use deterministic barriers to admit a pin between marking and deletion and
prove that the collector defers; pause a deletion batch and prove admission
cannot publish until it finishes; test admission to a removed generation;
exercise unreadable/absent epoch state, epoch-write crashes, process restart,
and an old binary that does not participate. Mutation proofs should neutralize
the epoch recheck and the deletion barrier independently and name the one
regression that fails for each control.
