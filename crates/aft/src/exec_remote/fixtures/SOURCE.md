Policy fixtures copied byte-for-byte from prefrontal commit `519c93ee4a5e`,
`test-vectors/exec-remote-v1/policy/`. The adjacent upstream README documents
the reference generator. Tests verify canonical bytes, SHA-256 digests and
every independent expected routing decision. Do not regenerate these from
AFT's matcher.

`plans/broca-worker*` comes from the same commit's
`test-vectors/fetch-plan-v1/plans/`. These are the actual frozen core plans;
catalog tests use their AFT tool item's params verbatim and verify the JCS
bytes against the published SHA-256, not against an AFT-generated expectation.

`frames/accepted.{json,jcs,sha256}` was copied byte-for-byte from Prefrontal
commit `01c3683c4`, `test-vectors/exec-remote-v1/frames/`, for caller types 0.2.1.
The JCS digest is `7ee77dd3db1fb24f5c4bf7af02ba1c8af39c9d5ecfcbb55dcf68f62954705d4c`.
All three copied files were checked against their matching entries in that
commit's `test-vectors/exec-remote-v1/SHA256SUMS`; the JCS was also checked
against `accepted.sha256`. These are the only newly vendored protocol files.
Older protocol outcomes/replies are read from the one locked published types
package, not duplicated here. The core policy/plans above retain their separate
519c93ee4a5e provenance; they are not the earlier motor-protocol corpus.
