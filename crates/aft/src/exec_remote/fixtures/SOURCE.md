`plans/broca-worker*` comes from prefrontal commit `519c93ee4a5e`,
`test-vectors/fetch-plan-v1/plans/`. Their `remote_exec.commands` lists date
from the command-prefix matcher AFT no longer has; AFT accepts and ignores
them, since a call now runs remotely only when it sets `runon`. That commit's
policy vectors tested the matcher and were removed with it. These are the actual frozen core plans;
catalog tests use their AFT tool item's params verbatim and verify the JCS
bytes against the published SHA-256, not against an AFT-generated expectation.

`frames/accepted.{json,jcs,sha256}` was copied byte-for-byte from Prefrontal
commit `01c3683c45b129ec50a93cae6210a5b8ad3ab06f`,
`test-vectors/exec-remote-v1/frames/`, for caller types 0.2.1. This revision
belongs to the CortexKit/prefrontal repository, not the AFT repository.
The JCS digest is `7ee77dd3db1fb24f5c4bf7af02ba1c8af39c9d5ecfcbb55dcf68f62954705d4c`.
All three copied files were checked against their matching entries in that
commit's `test-vectors/exec-remote-v1/SHA256SUMS`; the JCS was also checked
against `accepted.sha256`. These are the only newly vendored protocol files.
Older protocol outcomes/replies remain embedded in
`tests/fixtures/exec-remote/published-v0.2.0.json`, so unit tests do not run Cargo
or depend on its registry cache. All 26 outcomes and 16 replies, including their
original digests, were checked byte-for-byte against the locked published 0.2.1
package and are unchanged. The core plans above retain their separate
519c93ee4a5e provenance; they are not the earlier motor-protocol corpus.

The matching upstream `SHA256SUMS` entries are:

| File | SHA-256 |
| --- | --- |
| `frames/accepted.json` | `8e8f473e8a9a1138f6a6a3490a6dbb0eb7e55110cf98561588582c4ba3f9df40` |
| `frames/accepted.jcs` | `7ee77dd3db1fb24f5c4bf7af02ba1c8af39c9d5ecfcbb55dcf68f62954705d4c` |
| `frames/accepted.sha256` | `26a2ee885f7b0a9e2ad59451bd41c3768b363af9ebe0e2e9853a96fc3b6f2d43` |

`reports/crate-local-server-reports-{all,unchanged,older-runner,detached-head,truncated-untracked}.{jcs,sha256}`
were copied byte-for-byte from the published `cortexkit-exec-remote-types` 0.2.2
crate, `test-vectors/exec-remote-v1/outcomes/`. They cover a runner that
reports changed Git state, untracked files and ignored writes; one that reports
them all unchanged; an older runner that does not report them; a detached HEAD;
and a capped untracked list. Tests check each `.jcs` against its `.sha256`
before decoding it with the locked crate.
