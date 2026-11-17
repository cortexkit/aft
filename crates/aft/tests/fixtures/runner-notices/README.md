# Runner host-refusal wire fixtures

The JSON files contain the verbatim `reply` objects (including indentation) from
the `host_refused` cases in the published `cortexkit-role-llm-runner` 0.8.58
crate's `test-vectors/llm-runner-v1/` directory, also published with kit 0.17.69
(commons commit `beb81521`). Each extracted object has a final newline.

Upstream files and SHA-256 values recorded in the crate's adjacent `SHA256SUMS`:

| Fixture | Upstream file | SHA-256 of upstream file |
| --- | --- | --- |
| `send-host-refused.json` | `send.json` | `fdaf00d4ad7a20abf5269695a7dc9f3d5327019c601e9ddb9abd2eea2574688d` |
| `send-status-host-refused.json` | `send-status.json` | `ccc84ba5d936b8c32df30a39e73abb2a1ec8cc69057a6c765a5c1cc877d69db2` |

The fake runner wraps these bytes in the runner's `result` envelope without
re-serializing the reply, so both send and send-status paths exercise the
published wire fields with AFT's pinned 0.8.53 decoder.
