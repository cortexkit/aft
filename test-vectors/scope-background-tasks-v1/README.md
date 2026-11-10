# scope-background-tasks/v1

AFT advertises `scope-background-tasks/v1` beside `session-notices/v1` in
`manifest.capabilities.provides`. Discover this capability in the daemon catalog;
do not discover the provider by module name. Its management operation is
`scope.background_tasks`, with the `{op, params}` management envelope shown here.

The `scope` selector contains the scope owner's principal, opaque `ref`, and
`epoch`. The daemon-attested calling principal must equal `scope.owner`.
The refused sample uses the same request but an authenticated `direct` caller,
not the `reserved:prefrontal-core` owner. The refusal code is
`scope_owner_mismatch`. Request fields cannot assert the calling principal.

Each request and response is serialized by the Rust types used by the live
management handler; `scope_background_tasks_serializer_vectors_do_not_drift`
compares the exact bytes and SHA-256 hashes. The examples show an unfinished
remote command, a complete empty snapshot, an unavailable project registry, and
a refused non-owner. Optional `remote` contains the remote executor's job id.
Both queued and running local/remote tasks count as unfinished.

Commands and titles use `log_redact::redact_command`. `command` is limited to
2048 UTF-8 bytes without splitting a code point; a cut adds
`command_truncated: true`. The title is the same redacted, bounded command.

Reads do not wait for tool execution: registry/state locks are nonblocking,
saved-record and SQL iteration share a 4096-entry budget, and a one-second read
deadline is enclosed by a 1.5-second response deadline. Store errors, unloaded
registries, busy state, exhausted work budgets, and timeouts produce
`complete: false` with a `reason`. An incomplete empty list is not evidence that
a worker has no unfinished commands.

To intentionally regenerate the serializer samples, run the filtered Rust test
with `UPDATE_AFT_SCOPE_VECTORS=1`, and review all JSON and `SHA256SUMS` changes.
