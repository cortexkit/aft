# Runner send delivery samples

Vendored byte for byte from
`/Users/ufukaltinok/Work/Projects/CortexKit-scratch/shared-send-readers/`.
The source is Agent Runner's shared `llm-runner/v1` reader corpus. Each folder's
original README records its source operations and provenance; the JSON, README,
and `SHA256SUMS` files are unchanged. Every manifest entry was verified before
copying. The shared directory is a supplied snapshot, not a repository checkout;
the manifest hashes below identify its exact version. These are synthetic
recordings, not live user data.

| Folder | SHA-256 of `SHA256SUMS` | Manifest entries |
| --- | --- | ---: |
| `send-unknown-v2` | `8991bba0732b9c511c84ed714d14ba0ceb8caae19d1b25baaf79336fdd6332d5` | 9 |
| `send-answers-early-v1` | `5d6241426bf86020d36106fcf75a981d10d7f87376b50be5fa640250c06c8485` | 4 |
| `send-not-delivered-v2` | `e770d026b020b1c0ae5eb935f00af7969c7fe135499e75e0467ee62b650c5815` | 8 |

`runner_notices_shared_send_samples_have_pinned_bytes` pins these manifests and
checks every entry, including the upstream READMEs. The delivery-decision test
feeds all fifteen `session.send` / `session.send_status` bodies through AFT's
shared decoder and scheduler, as initial replies, follow-up replies (typed
status and same-ID replay), and best-effort reminders. A run ID without a receipt
does not settle delivery. A receipt, including `delivered.as: "unknown"`, settles
tracking without authorizing a resend; `not_delivered` permits a new generation
only for a completion.

The three other JSON bodies in `send-not-delivered-v2` describe `run.status`,
`run.result`, and `session.subscribe`. Their bytes are verified, but AFT does not
consume them as notice send replies.
