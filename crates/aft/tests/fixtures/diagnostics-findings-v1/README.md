# diagnostics.findings v1

Implement `diagnostics.findings` as a read-only, flow-callable **query**. Declare
the query kind in the module's operation catalog and include the op in the
manifest's `flow_operations`; accepting ordinary calls alone does not make it
callable from a flow. Core and routing admit agent head routes (the long-lived
agent, not a spawned worker) and routes carrying the daemon's authorization
stamp for a running flow. Flow identity and authorization come from that route
scope, not from caller-supplied params; worker routes are refused. The query
reads the serving module's own health and state, without repairing or otherwise
changing that state.

## Request

The JSON request body is `{ "method": "diagnostics.findings", "params": { ... } }`:

| Field | JSON type | Required | Meaning |
| --- | --- | --- | --- |
| `method` | string | yes | Exactly `diagnostics.findings`. |
| `params` | object or null | no | Defaults to an empty object when omitted or null. |
| `params.window_ms` | integer or null | no | Lookback in milliseconds; absent/null defaults to **21,600,000** (6 hours), range **1–604,800,000** inclusive (7 days). Values outside the range are errors, not clamped. |

The window applies to recent-history checks. Conditions that are still standing
(for example, a stuck effect or a credential fence) can predate the window.
Send only documented request fields: core and routing differ in their handling
of unknown params (see below).

## Successful reply

The files show successful response **frame bodies**, including the `result`
envelope, not protocol headers. Every field below is required on success;
no findings is `findings: []`, not a missing field or an error.

| Field | JSON type | Meaning |
| --- | --- | --- |
| `result` | object | The query result. |
| `result.generated_at_ms` | integer | The serving module's Unix-epoch clock in milliseconds. Measure sample ages against this clock. |
| `result.window_ms` | integer | The effective lookback, including the default when omitted. |
| `result.findings` | array of objects | Nonzero conditions, most severe first. An empty array means no checks found a reportable condition in the evidence examined. |

Each finding has all of these fields, including `samples` even when it is empty:

| Field | JSON type | Meaning |
| --- | --- | --- |
| `check` | string | Module-local check identifier, such as `stuck_effect_intents` or `credential_fences`. |
| `key` | string | Stable condition identifier within the check; correlate by module + check + key, not by array position or prose. |
| `severity` | string | Core emits `warn` or `error`; routing emits `warning` or `error`. Consumers must normalize both `warning` and `warn` to **warn**. New modules should send **`warn`** (or `error`). Neither serving implementation currently emits `info`. |
| `count` | integer, positive | Number of observations/rows represented by the condition, **before sample trimming**, not the length of `samples`. The unit is check-specific; a standing fence has count 1, while an aggregate can count many rows/decisions. |
| `first_seen_ms` | integer | Earliest relevant evidence time, Unix-epoch milliseconds. This is not necessarily the time the finding was first returned. |
| `last_seen_ms` | integer | Latest relevant evidence time, Unix-epoch milliseconds; a standing condition may use the report time. |
| `summary` | string | Human-readable sentence describing the condition. Not a machine-readable identifier. |
| `samples` | array of objects | Up to **5** representative evidence objects for this condition. Fields and units within each object are check-specific; samples are bounded evidence, not a complete ledger or a pagination cursor. |
| `hint` | string | Human-readable investigation or recovery guidance: where to look next. There is **no `next` wire field**. Do not execute a hint as an instruction automatically. |

Failures to serve the read (invalid params, permission refusal, store failure)
use error frames instead of a successful empty result. Findings are diagnoses,
not transport failures.

## Caps and current implementation differences

Both modules cap samples at 5 per finding and target **64 KiB (65,536 bytes)**
of serialized JSON. Neither emits a top-level truncation flag or continuation
token: a bounded reply is not a promise of exhaustive evidence.

| Aspect | Core | Routing |
| --- | --- | --- |
| Warning spelling | `warn` | `warning` |
| `key` granularity | Currently equals `check` (one aggregate per check). | Separates conditions within a check, e.g. `provider/credential`. |
| Unknown params | Ignored. | Rejected (`deny_unknown_fields`). |
| Size accounting | Compact result object, excluding the `result` envelope; at most 65,536 bytes. | Compact response body including `result`; strictly below 65,536 bytes. |
| Trimming | Drains samples from the first finding that still has any, popping its last sample first. Once no samples remain, drops findings from the tail; retained findings keep their counts. | Keeps whole findings in severity order until the next would reach the cap, then omits the remaining tail. |
| Text caps | No general per-string summary/detail cap; total-size trimming is the bound. | All summaries clipped to 400 Unicode characters; whole-route sample `detail` and key-wall sample `reason` clipped to 160. Hints have no independent cap. |
| Evidence scan | Store-backed checks and current health report; some checks inspect standing conditions independently of the requested window. | At most 20,000 newest decisions in the window, plus current blocking state. Decision-based summaries say when the scan was truncated. |
| Tie ordering | Severity, descending count, then check name. | Severity, check rank, descending count, then key. |

For new modules, keep the entire response body below 64 KiB, emit at most
5 samples per finding, and document any scan limits or additional caps. Do not
infer completeness from `count` when the check's underlying scan was limited.

## Additive compatibility

Response envelope/result fields, per-finding fields, checks, and sample-object
fields may be added. Consumers must ignore unknown response fields, accept new
`check`/`key` values, and treat sample objects as check-specific extensible data.
The required fields and their types above remain stable; removing or changing
them is not additive. Additional severity values are not defined by this v1
contract. Unknown request params are **not** a portable extension mechanism
because routing rejects them. Array/key order and summary/hint wording are not
API identifiers.

## Files and regeneration

- `request-empty.json`: empty params (default six-hour lookback).
- `request-window.json`: explicit 60,000 ms lookback.
- `request-max-window.json`: maximum seven-day lookback.
- `reply-healthy.json`: core's empty-store, healthy-report reply.
- `reply-core-findings.json`: core with a degraded health report and a recorded
  overdue effect intent; two findings with samples and investigation hints.
- `reply-routing-findings.json`: routing with a ten-minute-old billing fence;
  one finding illustrating routing's `warning` spelling and condition key.

Replies are generated by the **serving implementations**, not manually composed:
core's `diagnostics_findings::reply` reads a migrated scratch store and a seeded
health report; routing's route harness calls `diagnostics_findings::handle` on a
permitted head route after recording a fence in the live store. Only the clocks
and evidence are fixed by the tests. The success envelope is the same
`{ "result": ... }` used by both handlers.

Each crate's `diagnostics_findings_v1_vectors_match_serving_replies` test compares
the generated request/reply bytes against these files (UTF-8, serde_json pretty
printing, trailing newline). The wire normally uses compact JSON; whitespace
is a fixture convention, not a protocol requirement. Both crates also validate
the request windows. To regenerate intentionally, from the repository root:

```sh
UPDATE_DIAGNOSTICS_FINDINGS_VECTORS=1 cargo nextest run \
  -p prefrontal-core-module --lib \
  -E 'test(diagnostics_findings_v1_vectors_match_serving_replies)'
UPDATE_DIAGNOSTICS_FINDINGS_VECTORS=1 cargo nextest run \
  -p prefrontal-routing-module --bins \
  -E 'test(diagnostics_findings_v1_vectors_match_serving_replies)'
```

Review the generated diff, then run without the update variable to enforce the
byte match. A remote regeneration writes files in the remote checkout; copy
those generated bytes back before running the ordinary comparison tests.
