# Scoped Rust inspect: unfinished native analysis masked by compiler results

## Finding

This was a product false-complete, not an unsupported test assertion or a
flycheck publish arriving too late. With the installed `rust-analyzer 1.99.0
(b940084d 2026-09-28)`, native diagnostics arrive in the reply to
`textDocument/diagnostic`. `textDocument/publishDiagnostics` carries the
separate `rustc` results. A cancelled pull supplies no native analysis even
when the compiler subsequently publishes a versioned error for the same file.
A successful pull sent before the watched-file change is processed can also
return the old, empty native analysis. Neither result can certify the edited
workspace just because its compiler check later finishes.

## Traffic comparison

The unchanged settle logic was exercised with:

```sh
AFT_TEST_REQUIRE_RUST_ANALYZER=1 cargo test -p agent-file-tools --test integration scoped_rust_inspect_reports_a_removed_field_after_an_outside_edit_with_real_rust_analyzer -- --nocapture
```

Temporary instrumentation printed the reader's LSP responses, progress and
publish notifications, plus the scoped sweep's check state and final inspect
payload. No CPU load or longer timeout was used. The first instrumented run
failed; the second passed. Times below are relative to the after-edit pull
reply, rounded to milliseconds. The warm inspect completed a clean pull and
a flycheck begin/end in both runs.

| Event | Failing run | Passing run |
| --- | --- | --- |
| After-edit pull (request 3) | 0 ms: error `-32802`, `server cancelled the request`, `data: {retriggerRequest: true}` | 0 ms: full report, `resultId: rust-analyzer`, source `rust-analyzer`, `no such field` |
| Sweep's first poll | 103 ms: `Running`, push wait list empty | 1 ms: `Running`, push wait list empty |
| `$/progress` flycheck/0 begin (`cargo check`) | 1152 ms | 1087 ms |
| `publishDiagnostics`, `src/user.rs` | 1440 ms: version 0, source `rustc`, E0560, `struct S has no field named b` | 1440 ms: version 0, same `rustc` error |
| `$/progress` flycheck/0 end | 1469 ms | 1507 ms |
| Sweep declares check `Current` | 1821 ms | 1862 ms |
| Inspect diagnostics | One `rustc` error, file declared authoritative; native error missing | Both `rustc` and native errors |

Neither run published native diagnostics. In the failing run no native pull
was retried. The sweep placed the cancelled request in its retry list, then
discarded that obligation because a later push had advanced the file's
diagnostic epoch. The flycheck begin/end and existing publish settle interval
correctly described the compiler's completion, but did not answer the failed
native request. The overall response also had unrelated Tier-2 gaps; its
diagnostics coverage and `inspect_terminal: fresh` incorrectly certified the
Rust file.

Correcting the cancelled-pull retry alone passed nine consecutive runs, then
failed again. A second instrumented reproduction captured a **successful but
old** after-edit pull (request 3, full report, `resultId: rust-analyzer`, no
items). Relative to that reply, flycheck began at 1182 ms, pushed its version-0
`rustc` error at 1368 ms, and ended at 1384 ms. The server then requested
`workspace/diagnostic/refresh`, but AFT retained the earlier empty native
report. At 1727 ms the sweep called the compiler check `Current` and certified
the file with only the compiler error. Thus the request's timing, not merely
its success, matters: native analysis requested before the pending change/save
was processed is not completion evidence for the new workspace.

## Correction and regression coverage

Native pulls are deferred while Rust's check of the current workspace is
pending or running. Files are opened first so analysis/checks can proceed;
the native request is sent after the existing compiler-settle wait, not before
the watched-file change/save is processed. This uses the same original budget.
A push can still answer a failed pull for other servers. For Rust, a failed
pull must be retried with the **remaining original budget**, regardless of a
compiler push. A failed retry or exhausted budget leaves an `uncovered_file`
gap and diagnostics incomplete. Unsupported pull methods still take the
existing explicit push fallback. No deadlines or settle intervals changed.
Coverage also preserves the sweep's unanswered Rust requests even if a
compiler-only report is already stored. Previously the `Covered` verdict
short-circuited before the unanswered request's gap could be recorded.

The fake LSP's `AFT_FAKE_LSP_PULL_CANCEL=once|always` sends the observed
cancellation, flycheck begin, a versioned compiler-only publish, and flycheck
end, without timing load. The deterministic regressions check both outcomes:

- `scoped_blocking_inspect_retries_cancelled_rust_pull_after_compiler_push`
  requires the native pull diagnostic **and** the compiler error after retry.
- `scoped_blocking_inspect_marks_cancelled_rust_pull_unknown_despite_compiler_push`
  requires diagnostics incomplete and a named file gap when retry fails too.
- `scoped_blocking_inspect_waits_to_pull_native_rust_diagnostics_until_current_check_settles`
  uses `AFT_FAKE_LSP_PULL_WAIT_FOR_CHECK=1` to return the old empty native report
  until the emulated check finishes, and requires both native and compiler
  errors from inspect.

Before the correction, the first regression failed with
`("fake-lsp", "test pull diagnostic") missing` and compiler-only authoritative
coverage. Reverting the Rust exception after the correction reproduces that
failure; disabling the unknown-coverage obligation and native-pull deferral
also reddens their respective regressions. The ordinary pull-server control
remains green. The original real
analyzer outside-edit assertion is unchanged and passed 20 consecutive runs
with `AFT_TEST_REQUIRE_RUST_ANALYZER=1` after the correction.
