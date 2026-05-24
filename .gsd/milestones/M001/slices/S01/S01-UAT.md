# S01: Binary Scaffold & Persistent Protocol — UAT

**Milestone:** M001
**Written:** 2026-03-14

## UAT Type

- UAT mode: live-runtime
- Why this mode is sufficient: S01 is a process scaffold — the proof is that the binary runs, accepts commands, and stays alive. All verification requires actually running the binary.

## Preconditions

- Rust toolchain installed (`cargo` and `rustc` available)
- Repository checked out with all S01 source files present
- No other process holding a lock on `target/` directory

## Smoke Test

Run `echo '{"id":"1","command":"ping"}' | cargo run` from the project root. Expected: stdout contains `{"id":"1","ok":true,"command":"pong"}`, stderr shows `[aft] started` and `[aft] stdin closed, shutting down`.

## Test Cases

### 1. Ping health check

1. Run `echo '{"id":"h1","command":"ping"}' | cargo run 2>/dev/null`
2. **Expected:** stdout is exactly `{"id":"h1","ok":true,"command":"pong"}`

### 2. Version identification

1. Run `echo '{"id":"v1","command":"version"}' | cargo run 2>/dev/null`
2. **Expected:** JSON response with `"id":"v1"`, `"ok":true`, and `"version":"0.1.0"`

### 3. Echo round-trip with arbitrary params

1. Run `echo '{"id":"e1","command":"echo","message":"hello","count":42}' | cargo run 2>/dev/null`
2. **Expected:** JSON response with `"id":"e1"`, `"ok":true"`, and the params echoed back (`"message":"hello"`, `"count":42`)

### 4. Sequential command throughput

1. Generate 100+ JSON commands with incrementing IDs: `for i in $(seq 1 110); do echo "{\"id\":\"$i\",\"command\":\"ping\"}"; done | cargo run 2>/dev/null`
2. Count output lines: pipe to `wc -l`
3. **Expected:** exactly 110 response lines, each valid JSON with matching `id` and `"ok":true`

### 5. Malformed JSON recovery

1. Send a multi-line input where line 1 is `not valid json` and line 2 is `{"id":"ok","command":"ping"}`:
   ```
   printf 'not valid json\n{"id":"ok","command":"ping"}\n' | cargo run 2>/dev/null
   ```
2. **Expected:** Two lines of output. First line is an error response with `"id":"_parse_error"` and `"ok":false`. Second line is the successful ping response with `"id":"ok"` and `"ok":true`.

### 6. Unknown command error

1. Run `echo '{"id":"u1","command":"explode"}' | cargo run 2>/dev/null`
2. **Expected:** JSON error response with `"id":"u1"`, `"ok":false`, `"code":"unknown_command"`, and `"message"` containing `"explode"`

### 7. Clean shutdown on stdin EOF

1. Run `echo '{"id":"1","command":"ping"}' | cargo run 2>/tmp/aft_uat_stderr.txt`
2. Inspect stderr: `cat /tmp/aft_uat_stderr.txt`
3. Check exit code: `echo $?`
4. **Expected:** exit code 0, stderr contains `[aft] started, pid` and `[aft] stdin closed, shutting down`

### 8. Stderr diagnostics for parse errors

1. Run `echo 'garbage' | cargo run 2>/tmp/aft_uat_stderr.txt 1>/dev/null`
2. Inspect stderr: `cat /tmp/aft_uat_stderr.txt`
3. **Expected:** stderr contains `[aft] parse error:` with the raw input `garbage` mentioned

### 9. Request ID preservation

1. Send commands with unusual IDs: `echo '{"id":"uuid-abc-123","command":"ping"}' | cargo run 2>/dev/null`
2. **Expected:** response `id` is exactly `"uuid-abc-123"` — not modified, truncated, or missing

### 10. Empty line handling

1. Send input with empty lines between commands:
   ```
   printf '\n\n{"id":"1","command":"ping"}\n\n' | cargo run 2>/dev/null
   ```
2. **Expected:** exactly one response line for the ping command. Empty lines are silently skipped.

## Edge Cases

### Missing id field

1. Run `echo '{"command":"ping"}' | cargo run 2>/dev/null`
2. **Expected:** error response with `"id":"_parse_error"` and `"ok":false` — the binary does not crash

### Missing command field

1. Run `echo '{"id":"1"}' | cargo run 2>/dev/null`
2. **Expected:** error response — missing `command` field produces a parse error or unknown command error. Binary continues running.

### Very large echo payload

1. Send an echo command with a large params object (e.g., a string value of 10,000 characters):
   ```
   python3 -c "import json; print(json.dumps({'id':'big','command':'echo','data':'x'*10000}))" | cargo run 2>/dev/null
   ```
2. **Expected:** response contains the full 10,000-character string echoed back. No truncation.

### Rapid sequential commands (stress)

1. Run `cargo test --test integration -- test_sequential_commands --nocapture`
2. **Expected:** test passes, output shows 120 commands processed

## Failure Signals

- Binary crashes (exit code != 0) after malformed input
- Response missing `id` field or `id` doesn't match request
- Stdout contains non-JSON output (diagnostic messages leaking from stderr)
- Empty lines in stdin cause error responses instead of being silently skipped
- Process hangs instead of exiting on stdin EOF
- `cargo build` produces warnings
- `cargo test` has any failures

## Requirements Proved By This UAT

- R001 (Persistent binary architecture) — tests 1-4, 7 prove the binary runs as a persistent process with JSON stdin/stdout and clean shutdown
- R032 (Structured JSON I/O) — tests 1-6, 9 prove all communication is JSON, request IDs preserved, structured errors
- R031 (LSP-aware architecture) — partially: protocol types include optional lsp_hints field (proven by unit tests, not UAT)

## Not Proven By This UAT

- No real file operations — ping/version/echo are bootstrap commands only
- No tree-sitter parsing — LanguageProvider is a stub (S02)
- No checkpoint/restore — safety system not yet implemented (S04)
- No cross-platform behavior — UAT runs on the build machine only (S07)
- Memory leak detection over extended sessions — not tested here (would need valgrind/heaptrack)

## Notes for Tester

- All commands use `2>/dev/null` to suppress stderr cargo build output. For debugging, remove it.
- The binary must be built first (`cargo build`) or use `cargo run` which builds automatically.
- Test 4 (sequential throughput) may take 1-2 seconds — that's normal for 110 process spawns worth of cargo run overhead. The integration test (edge case: rapid sequential) is the real throughput test against a single process.
- The `lsp_hints` field in request JSON is optional and currently ignored — including it should not cause errors.
