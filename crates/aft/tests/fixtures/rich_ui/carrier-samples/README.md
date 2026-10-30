# Phone carrier read-gate wire samples

Captured from main commit **9782a74b254bdfbeeb0c7e09909b8d888609ffb1**
using the production handlers and route admission code at that commit. The
capture adds tests only; it does not change the gate or the tool implementations.

These are `tool.call` frames on the untrusted `reserved:occipital` carrier route,
negotiating `tool-result/v1`, with the `worker` preset and bound session
`session-s`. They are not the separate, projected `rich_ui` operation results in
the adjacent schema-freeze fixtures. In particular, the task tools here take
`taskId`, not the rich-UI viewed-call `task` binding.

Successful replies have tagged `tool-result/v1` bodies. Admission refusals are
protocol `Error` frames with `code` and `message`, even when `tool-result/v1`
is negotiated; they are not fabricated tool-result error envelopes.

## Cases

| File | Request | Expected outcome |
| --- | --- | --- |
| `read.json` | Read `main.rs` | Success |
| `aft_outline.json` | Outline `main.rs` | Success |
| `aft_zoom.json` | Zoom to `greeting` in `main.rs` | Success |
| `aft_search.json` | Search for literal pattern `greeting` | Success |
| `grep.json` | Grep Rust files for `greeting` | Success |
| `glob.json` | Glob `*.rs` | Success |
| `aft_callgraph.json` | Find callers of `greeting` | Success |
| `bash_status.json` | Observe a completed, session-owned task | Success |
| `bash_watch.json` | Watch that completed task | Success |
| `refused-edit.json` | Call non-admitted `edit` | `carrier_read_only` |
| `refused-web.json` | Outline an HTTPS target | `carrier_read_only` |
| `refused-github-issue.json` | Read an `issue://` target | `carrier_read_only` |
| `refused-github-pr.json` | Read a `pr://` target | `carrier_read_only` |
| `refused-invalid-arguments.json` | Read with an unknown argument | `invalid_request` |

The read-only gate has no numeric argument limit of its own. The invalid case
captures the tool-provider schema admission refusal after the read-only gate.
HTTPS **read** is not used as the web refusal: this gate specifically rejects
web targets for outline/zoom, and GitHub targets for read/outline/zoom.

## Fixture and capture boundary

Each case gets an isolated project containing only this `main.rs`:

```rust
fn greeting() -> &'static str {
    "hello phone"
}

fn main() {
    println!("{}", greeting());
}
```

Trigram and callgraph indexes are ready before each request; semantic embedding
is disabled. The task cases observe persisted stdout `hello phone\n`, empty
stderr and exit code 0. The task has a fixed 12 ms duration; no shell process or
network request is needed.

The tests reuse the existing carrier-gate route harness: an actual request
`Frame` enters `handle_tool_call`, reaches real command handlers, and passes
through final `project_tool_result()` serialization. This is an in-process
route capture, not a live phone/socket session or a fabricated handler response.

## JSON frame representation and normalization

Each file contains `request_frame` and `reply_frame`. Each frame has:

- `header_hex`: the complete binary frame header, hex encoded.
- `body_utf8`: the complete serialized JSON body as a string, **not** a parsed
  and reserialized body object. Parse this string to consume the tagged result.

To recover frame bytes, hex-decode `header_hex` and append the UTF-8 bytes of
`body_utf8`. JSON escaping in the outer sample file is only the storage wrapper;
the inner body's key order, whitespace, text and envelope fields are preserved.

Normalization is limited to:

- The temporary fixture directory becomes `${FIXTURE_ROOT}` in body text.
- Task `started_at`/`finished_at` timestamps, when present, become numeric `0`.
- Only for `bash_watch`, measured `waited.elapsed_ms` becomes numeric `0`, and
  the corresponding rendered `Waited Nms` becomes `Waited 0ms`. This is measured
  observation time, not the fixture task's fixed `duration_ms` of 12.

The header's body length is recomputed after normalization. Frame channel 41,
epoch 1, correlation 7, derived request id `subc-41-7` and task id
`bash-0123456789abcdef` are fixed by the harness, not rewritten. No fields are
dropped; all other messages, result data and durations are preserved.

## Platform variants

All fourteen cases were captured natively on Windows. Only `bash_status`
differs from the common samples:

| Windows variant | Preserved platform difference |
| --- | --- |
| `bash_status.windows.json` | Task status includes `· live descendant check n/a on Windows (background tasks do not use a Job Object)`. |

`bash_watch` and the other twelve cases match their common samples after the
existing normalization, so they have no Windows variants.

On Windows, the normal test selects `<case>.windows.json` when that variant
exists; otherwise it uses the common `<case>.json`. Other platforms always use
the common sample. Platform-specific reply text is preserved, not normalized.

The native Windows capture reruns all fourteen cases and compares each complete
normalized frame pair with its common sample. It writes a `.windows.json` file
only when the pair differs, and prints `CARRIER_SAMPLE_UNCHANGED <case>` for an
identical pair. It never overwrites the common JSON files on Windows.

## Reproduce and review updates

Run from the repository root on Linux (the worker guide requires remote Cargo
jobs; print `git log -1` first and use 1 or 2 vCPUs):

```sh
git log -1
cargo test -p agent-file-tools --lib -- carrier_samples::capture_carrier_samples --ignored --nocapture
cargo test -p agent-file-tools --lib -- carrier_samples::carrier_samples_match_wire
```

Run the same capture and comparison commands natively on Windows using
`runon: "windows"` to regenerate or verify Windows variants; cross-compilation
cannot capture platform-specific replies.

On non-Windows platforms, the ignored capture test explicitly overwrites the
fourteen common JSON samples. On Windows, it writes only differing variants.
It prints each written sample as `CARRIER_SAMPLE <case>[.windows] <json>` for
export from remote jobs.
The normal test never updates fixtures: it reruns all cases, validates successful
tagged results and admission error codes, and compares normalized frames,
including the exact serialized body strings, to these reviewed samples.
