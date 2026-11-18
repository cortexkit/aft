# AFT status KV v1 samples

These are recorded outputs of AFT's real status key and value encoders, before
enabling any live status KV puts. `*.key` contains the exact KV key bytes and
`*.json` contains the exact value bytes, without trailing newlines. `SHA256SUMS`
covers those six files.

| Sample | Runner session id | Segment | Generation |
| --- | --- | --- | --- |
| ascii | `session-1` | `[AFT E0 W1 \| T2]` | 1 |
| punctuation | `runner:one/two` | `[AFT E2 W0 \| T3]` | 42 |
| unicode | `会話:é/🦀` | `[AFT E? W0 \| T1]` | 1700000000001 |

The key is lowercase hex of the session id's UTF-8 bytes, not a project id, a
scope id, or a hash. The JSON envelope is `v: 1`, the unmodified AFT segment, and
an integer generation. A refresh repeats the same generation and bytes. Status
changes increment the generation; production seeds counters from AFT's process
start time in milliseconds. That restart fence assumes the wall clock does not
step backwards across a restart and a key changes no faster than once per
millisecond over the process's lifetime.

The bucket declaration comes from `cortexkit-bus-naming` 0.4.3 `MODULE_BUCKETS`
(owner `aft`, name `status`), not a hard-coded bucket name. The bus owns its 10 s
TTL and 2,048-byte value limit. AFT puts changes and refreshes live sessions
every 5 s, stops when their last inbound route closes, and skips oversized JSON
values without truncation. The existing status-holder route still carries AFT's
segment when KV is unavailable.

The unit test `late_result_bus::status::tests::status_kv_recorded_vectors_match_real_encoder`
regenerates the keys, JSON and checksums in memory and byte-compares all files.
To intentionally re-record them, run that filtered lib test with
`AFT_RECORD_STATUS_VECTORS=1`; review every changed byte before committing.
