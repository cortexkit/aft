# AFT rich UI v1 schema freeze — CKIOS handoff

This directory is the schema-freeze handoff. It is **not a publication**: the
production builder returns `None`. S denotes the session whose page is being
shown, carried under its stamped owner/ref/epoch. The catalog digest and both template sources
are test-only placeholders. No placeholder is reachable through production.

## Frozen operation contract

`field-table.json` pins all ten version-1 entries, including args, result/item,
bound entities, profiles, targets, limits and paging. Every result field has its
source, presence, nullability and placement recorded. `manifest.jcs.json` is the
canonical manifest; `manifest.sha256` is its committed digest. `bash-pins.json`
pins every distinct served Bash definition (Head/Worker, with/without runon).
The Bash page's reads equal the placeholder page template's reads.

| Operation | Target | Result |
|---|---|---|
| bash_status | bash_status | `{running, exitCode}` |
| bash_output | bash_status | `{items: [{stream, text, bytes, continued, firstAvailableOffset, prefixDropped}], next}`; never total |
| bash_watch | bash_watch | `{running, exitCode, waited: {reason, limit_ms}}` |
| read | read | `{text}` |
| outline | outline | `{text}` |
| zoom | zoom | `{text}` |
| search | search | `{text}` |
| grep | grep | `{text}` |
| glob | glob | `{text}` |
| callgraph | callgraph | `{text}` |

Session-read text is the dispatcher-formatted handler reply (`data.text`), not
an arbitrary handler object. Keeping a single textual field gives the seven
union-free read operations a common result without exposing changing backend
metadata or recursive callgraph/zoom shapes. It does not add `text` to a shell
success. Shell `running` is the inverse of `BgTaskStatus::is_terminal`; a missing
or unknown status is a validation failure, not a default. `exitCode` maps the
required nullable `exit_code`. Watch output previews, pattern matches and worker
kill/wait metadata are not published. Poll reasons are match, exit, timeout.

`result-vectors.json` and `handler-replies.json` remain schema-only vectors;
the generated samples below exercise real handler/outer-route replies against
the same schemas. Projections drop unknown keys and never default a required field.

## Generated handler samples

`aft-rich-ui-manifest-v1.json` captures requests and complete carrier reply
bodies for all ten operations, plus an ordinary handler error. Unlike the
schema-only vectors above and the legacy `carrier-samples/` captures, these
requests carry `rich_ui_op` and call-scoped requests bind the viewed call with
`task`. They run through the real outer route and production handlers under a
core-head policy route, using the **injected test manifest**, not a published
production manifest. The project has a fixed two-function Rust source file,
ready trigram/callgraph indexes and no semantic embedding service. Bash replies
observe a persisted terminal task with stdout `one\n`, stderr `two\n` and exit
code 0; no shell command or network request is run.

`manifestDigest` identifies the injected manifest bytes. It is not a production
publication tag; regenerate and review the samples when publishing the manifest.

Normalization replaces the temporary project root with `${FIXTURE_ROOT}`, the
fixture task id with `bash-SAMPLE`, and the viewed call key with `sample-CALL`.
Numeric task timestamps and measured durations, if present, become 0. The
projected Bash schemas currently omit these volatile fields. Exit codes, byte
counts, wait limits, result fields and error messages are not normalized.

Regenerate from the repository root:

```sh
cargo test -p agent-file-tools --lib -- regenerate_rich_ui_samples --ignored --nocapture
```

The normal `rich_ui_samples_match_handlers` test recaptures replies, validates
their schemas and exact carrier body shape, and compares the normalized bytes
with the committed file. Regeneration is opt-in and never happens in that test.
`WORKER.md` lists both the regeneration command and the filtered drift check.

## Inputs, targets, media, messages

Call-scoped args never contain a viewed task. `bound-task.json` freezes the
carrier-only `task` argument as the **viewed Bash call key**, not a task id or
frame key. `bash_status.args` is the empty record. `bash_output` accepts cursor
and limit (1–200). Watch accepts pattern, optional regex, optional timeoutMs
(1–20,000). Session args use only union-free forms: outline target is a string;
zoom has a batch of targets, each with a list of symbol strings. Empty batches
and symbol lists fail the served-schema check, not a fabricated schema union.

`target-arguments.json` records every target-bearing field and the precedence
between the read-only gate and the carrier's argument validation (admission
step 5: size, closed schema and remote-target checks) for URL, issue and PR forms, including outline arrays (which may be
refused before their schema check) and nested zoom targets. `ref` is excluded.
Search patterns, grep include patterns and glob patterns are not target fields.

`gate-contract.json` records evidence from **20aba8d97b, landing**:
`20aba8d97b65c788759f9ee5024d71fdb98133f5`, file
`crates/aft/src/subc/carrier_read_only.rs`. That commit is absent from the
8072df9 base; its gate is being landed separately. The canonical public gate
names include `aft_outline`, `aft_zoom`, `aft_search`, `aft_callgraph`; they map
to the bare target names above. Nothing in this slice copies, widens or runs the
gate. Its tool refusal comes before target refusal. Both use
`carrier_read_only`. Read HTTP(S) targets pass that gate but fail step 5 with
`args_invalid`; outline/zoom HTTP(S), and all three tools' issue/PR targets, fail
at the gate. Other operations' target paths fail step 5. The gate's two message
templates are preserved verbatim in the fixture, with its substitution markers.

`media-outcomes.json` has one case per image/PDF/binary × read/outline/zoom.
All nine refuse **unsupported_media** before handler execution. v1 does not
carry attachments or pretend a size/attachment summary is file content.
Ordinary text/directory reads are unaffected. The carrier-serving read slice
must install the media check before calling these handlers.

Fixed safe messages and the generic fallback are frozen in
`crates/aft/src/subc/rich_ui/operations.rs` (`FIXED_MESSAGES`, `GENERIC_MESSAGE`);
each is at most 1,024 UTF-8 bytes. Unknown codes pass through with generic text.
Handler messages, data and original error output are excluded from carrier
error replies. No session metadata (`status_bar`, `bg_completions`) is included
in any carrier reply.

## Transport, frame identity and binding

`transport-contract.json` posts:

- Chunk requests: `rich_ui.manifest_chunk {manifestDigest,index}` and
  `rich_ui.template_chunk {templateDigest,index}`, 0-based.
- Chunk replies: exactly `{digest,index,chunks,bytes,data}`, where bytes is the
  whole asset length and data is standard padded base64 of ≤131,072 bytes.
  All refusals are typed protocol Error frames carrying a code, **not** a
  carrier tool-result body; neither messages nor frames echo request data.
- Carrier body: exactly `{content,isError,structuredContent}`. Content is one
  empty text item on success, one text item containing the code on failure.
  Success structuredContent is only the validated projected result or page;
  failure is exactly `{success:false,code,message}` with fixed safe text.
- **Fresh frame key per read, or no key.** Top-level call_key belongs only to
  the carrier frame. `arguments.task` names the viewed call. Reusing a settled
  frame key replays the same body, with the retry's correlation header, not a
  fresh poll. The carrier ledger string is `reserved:occipital#` followed by the
  compact JSON array `[owner principal_id,scope_ref,epoch decimal string]`;
  ledger scope is None. S's policy-route principal is never the ledger carrier,
  and the ledger row's scope is never S's scope.
- **Stamp equality, not label equality.** A carrier route serves S only when
  its daemon-stamped `(owner, ref, epoch)` equals S's policy-route stamp. The
  daemon attests the same stamp on both routes. Route labels (`project_root`,
  `harness`, `session`) are always the opener's own unattested claims.
  Occipital sends neutral labels of its own; AFT ignores those labels and the
  carrier's project_root, so there is no label comparison.
- **Policy-route source.** S's project, read rules, harness and session come
  only from S's own live policy route: core's relay for a head, Broca's tool
  route for a worker. The project is checked against the stamp's
  `workdir`/`project_id` under `scope-workdir/v1`. The Bash task table uses S's
  harness and session from that policy route. No live policy route under S's
  stamp means `session_not_bound`. Unscoped and self-owned scoped Occipital
  routes serve provider reads only.

**CKIOS accepted the freeze** in Rich UI room seq 105, following handoff seq
104 (review/rich-ui-s02-freeze at d10f0710b5). The label-source acceptance item
is **resolved by stamp equality**, agreed in SUBC seq 109, CKIOS seq 107 and AFT
seq 110. That rule supersedes the original bind-label equality handoff: no
source of S's labels is needed by Occipital. CKIOS supplies the real row/page
templates against this freeze as canonical files with SHA-256s and catalog
digest; the promised handover is 2026-10-26T02:00Z.
Publication also waits for CKIOS canonicalization vectors, all carrier operations
and validation, approval of agent-visible refusal wording, SUBC capability
confirmation and the pinned daemon's HELLO test. This handoff does not attach a
HELLO descriptor, serve carrier reads/chunks or enable production view records.

## Verification / regeneration

Run `cargo test -p agent-file-tools --lib -- subc::rich_ui`. To review a deliberate
new operation-version freeze, run that test with `AFT_RICH_UI_PRINT_FREEZE=1`
and `--nocapture`; the golden test prints candidate canonical bytes, digest and
pins before comparing them with committed bytes. Updating fixtures is an
explicit review action, never automatic on a passing test. Do not update an
existing frozen version just to accept a changed field.

The nested `.gitattributes` keeps JSON/template/digest bytes unchanged on
Windows checkouts. JSON assets are embedded using `include_bytes!`. The same
unit digest assertion runs on Linux, macOS and Windows CI jobs.

## `canonical_vectors.json`

RFC 8785 (JSON Canonicalization Scheme) vectors plus the manifest number rule.
Each entry is:

| key | meaning |
|---|---|
| `name` | what the case pins |
| `input` | the JSON value to canonicalize |
| `canonical` | the exact canonical bytes as a string, or `null` when the manifest number rule must refuse the input |
| `digest` | `sha256:` of `canonical`, or `null` when `canonical` is `null` |

The second case reproduces RFC 8785's section 3.2.3 property-sorting example.
The first is RFC 8785's section 3.2.2 example with its fractional numbers
replaced by the integers they round to, because a manifest object carries
integers only. The remaining cases pin member sorting by UTF-16 code unit,
string escaping, preserved array order, the manifest number rule's exact
±(2^53-1) bounds, and its refusals for a fractional and an out-of-range
number.

## Terminal `aft.bash.view` v1 record

`view-record.json` freezes the full closed record schema, field provenance and
vectors; future record-builder tests must validate their results against it.
The manifest deliberately keeps only `viewRecord: {schema, version}`: the
closed manifest language has no member for an embedded view-record schema.
The original task id, original Bash pin and stored template digests remain in
the record. All fields below are required; nullable means an explicit null is
allowed, not that the key may be omitted. Counts are bounded safe integers.

| Field | Type / values | Source |
|---|---|---|
| schema / version | `"aft.bash.view"` / `1` | fixed record identity |
| task_id | string ≤1,024 bytes, nullable | `PersistedTask.task_id`; snapshot `info.task_id`; non-null when emitted by AFT |
| schema_pin | string ≤1,024 bytes, nullable | original Bash `ToolCallRequest.schema_pin`, carried with the task, not the observing status/watch pin; non-null when emitted by AFT |
| phase | `running` / `finished` | host may supply running placeholders; AFT emits finished only |
| exitCode | integer ±(2^53−1), nullable | snapshot / persisted `exit_code`; unknown stays null |
| durationMs | integer 0…2^53−1, nullable | snapshot / persisted `duration_ms`; unknown stays null |
| outputBytes | integer 0…2^53−1, nullable | task-row `output_bytes`: retained stdout + stderr file lengths, or PTY file length; null if neither length is readable |
| endReason | `completed`, `failed`, `killed`, `timed_out`, `fate_unknown`, nullable | terminal `BgTaskStatus`, not free-text status_reason; non-null when emitted by AFT |
| templates | `{row, page}`, each exactly `sha256:` plus 64 lowercase hex digits (71 ASCII bytes) | first-write-wins stored digests of canonical published template files selected with the original Bash pin |

The interchange schema admits a host running value with task_id, schema_pin,
endReason, exitCode, durationMs and outputBytes all null. **AFT-emitted records
are still finished only**, with task_id, schema_pin and endReason non-null.
`operations::validate_view_record` enforces that emission rule and phase-dependent
nulls in addition to the bounded schema. It also checks both template digests
with `valid_template_digest`: wrong length, prefix, uppercase or non-hex text
refuses `result_schema_mismatch`. Digest spelling cannot be expressed by the
bounded language (which has no patterns), so this explicit check is mandatory.
The vectors' digests equal the builder's published template-file hashes.

Code sources: `crates/aft/src/bash_background/mod.rs:908-943` defines snapshot
info and terminal states; `registry.rs:687-755` defines the snapshot;
`persistence.rs:1549-1673` defines persisted task data, and
`persistence.rs:1804-1842,1932-1946` captures retained byte lengths into the task
row. Windows remote exit codes may exceed i32::MAX
(`persistence.rs:1581-1583`), so the status/watch/view schemas preserve those
values within the manifest's safe-integer range.

There is **no foreground/background/promoted mode field**: persisted capture
mode is only `pipes` or `pty` (`persistence.rs:1534-1540,1565`). It is not a
three-way launch history, and a terminal observer cannot reconstruct that
history reliably after restart. Every record observes a terminal task, even
if that task originally ran in the background; its actual exit code is not
blanked merely because of its launch style. Running/background presentation
and start time come from the host's display state before a terminal record,
not from invented fields on the record. Memory-cap narratives and
`scope_revoked` status reasons are not additional `BgTaskStatus` variants;
`endReason` deliberately exposes only the closed terminal-state codes, without
parsing or forwarding free text. No command, output, preview, match text or
arbitrary reason text appears in the record.

## Bash input fields for CKIOS

`bash-inputs.json` is checked against Head/Worker definitions with and without
runon. `input.command` exists and is required in all four; `input.description`
and `input.workdir` exist but are optional in all four. `input.runon` exists
only in the two runon-enabled variants, where it is optional. It is absent
from both no-runon variants. The page must not unconditionally access it for
those pins: omit the runon label or show field-unavailable/no explicit target,
and never synthesize a runner. An absent input field is not evidence of a
particular execution host. The source is the embedded catalog assembled by
`tool_provider.rs::build_tools/tools_for_session`; the unit test checks the
actual served definitions and the golden Bash pin list.

## Line-oriented `bash_output` and retention dependency

CKIOS's page polls status every two seconds and uses output pages of up to 200
items. It does not need `bash_watch`; that operation remains in the manifest.
The long-line raw input in `bash-output.json` uses `repeatByte`, `count` and
`suffix` to avoid storing thousands of identical bytes. The test loader expands
only the input and checks its original SHA-256; expected page text and splits
remain literal, independently frozen values.

`bash-output.json` freezes line, cursor and retention rules plus source-byte
vectors. These rules supersede the earlier whole-unit chunk-run item sketch,
before publication. Each output item now has:

| Field | Type | Meaning |
|---|---|---|
| stream | stdout / stderr / pty | source artifact |
| text | string ≤8,192 UTF-8 bytes | one line, or one part of an oversized line, without LF/CRLF terminator; bare CR is text |
| bytes | integer 1…8,192 | original bytes consumed; final part includes the line terminator's bytes |
| continued | boolean | true on every non-final part of that physical line; false on the final part |
| firstAvailableOffset | integer 0…2^53−1, nullable | original byte offset of the first retained byte of this stream; repeated per item |
| prefixDropped | boolean | whether any original prefix was dropped |

Lines are split only at whole UTF-8/lossy-decode unit boundaries, under both
text and source-byte caps. A running unterminated suffix is held until a
terminator/terminal EOF is known or lookahead proves another nonempty part
follows; no zero-byte closing items are invented. The table can display items
directly and mark continuation parts without re-splitting text.

Only **per-stream** byte order is recoverable. The backend stores stdout and
stderr separately (`commands/bash_status.rs:244-305`); each poll reads at most
one 64 KiB backend page per stream, emits stdout parts then stderr parts and
leaves a deferred stream's offset unchanged. This is not chronological
stdout/stderr interleaving. PTY has one combined stream in capture order.

A running page always has a task-bound opaque cursor, even at the current end
or while holding an unclosed line. The cursor authenticates per-stream positions
and a monotonic read sequence (keyed MAC or task-bound encoding, bound to the
viewed task and session). Each successful read increments that sequence, so
**next differs from the request even when positions have not changed**. There
is no sequence wrap or per-read database counter. Reading next later can return
appended output; it is null only after terminal status and full drain of both
streams. Tampered, foreign-task or malformed cursors refuse `malformed_cursor`
with the fixed message in operations.rs. No `total` is emitted. Symbolic
no-progress, progress, terminal-drained and refusal vectors in bash-output.json
pin the contract, not a particular future cursor encoding. Frame-key replay
still reuses the stored body/cursor; fresh evaluation needs a fresh frame key.
The existing backend re-samples artifact length for each read and advances by
returned bytes (`commands/bash_status.rs:403-430`); the later carrier output
layer, not `output_has_more` alone, decides terminal drain.

**The beginning is not retained indefinitely.** At terminal handling,
`bash_background/buffer.rs:14,356-369` trims each artifact to a 100 MiB tail.
`persistence.rs:4380-4398` atomically replaces it with a UTF-8-aligned tail.
The returned prefix-loss count currently enters transient recovery text
(`registry.rs:13018,13145-13149`), not a durable per-stream original-offset
base. Thus current retained-file offset zero is not always original byte zero.

**Explicit dependency for the later output/storage slice:** persist a cumulative
per-stream dropped-prefix base in the task record whenever a trim runs, and
retain it across restarts. New untrimmed streams use base 0 and
`prefixDropped:false`; tracked trimmed streams use their exact positive base
and `prefixDropped:true`. Only tasks trimmed before tracking existed may use
`firstAvailableOffset:null`, always with `prefixDropped:true`; never report
zero for a trimmed stream. Cursors use original offsets once tracked; positions
older than the retained base advance to that base. Legacy unknown-origin
cursors must remain explicitly unknown/retained-relative rather than claim
original zero. A first page starts at original byte 0 only when untrimmed,
otherwise at the first retained byte. The page must label a tail/unknown origin
and never pretend that it starts at original line 1.
