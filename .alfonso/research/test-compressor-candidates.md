# AFT bash compression research: test/lint/build tools with mid-run failures

Context: AFT has a 30KB inline output cap and generic compression middle-truncates. Tools below are ranked by how often their default output can put rich failure/diagnostic blocks before a final summary, so the summary survives but actionable detail is lost.

Existing Rust compressors observed in this worktree: `cargo`, `eslint`, `vitest` (also matches `jest` tokens), `biome`, `pytest`, `tsc`, `git`, `npm`, `bun`, `pnpm`. Current `bun test` path falls back to generic compression.

## Ranked recommendations

### 1. Bun test
- **CLI**: `bun test`, `bun test --watch`.
- **Failure block to preserve**:
  ```text
  path/to/example.test.ts:
  12 | expect(actual).toBe(expected)
       ^
  error: expect(received).toBe(expected)

  Expected: 42
  Received: 41
      at <anonymous> (path/to/example.test.ts:12:18)
  ```
  Also preserve lines like `fail: suite > test name`, thrown exception stacks, diffs, and `AggregateError`/snapshot blocks.
- **Prevalence**: 4/5 in modern JS/TS repos using Bun.
- **Existing shared logic**: No. `bun.rs` currently sends `bun test` through generic compression; Vitest/Jest `FAIL ...` matching is not sufficient for Bun's file/error/diff shape.
- **Truncation risk**: High. Failure detail appears after many pass lines and before `Ran N tests across M files. [X failed]`; final summary does not include assertion diffs/stacks.
- **Recommendation**: Highest priority; immediate trigger and not covered today.

### 2. Go test
- **CLI**: `go test ./...`, `go test -v ./...`, `go test -race ./...`.
- **Failure block to preserve**:
  ```text
  --- FAIL: TestParser (0.01s)
      parser_test.go:42: got "a", want "b"
      parser_test.go:43: diff (-want +got):
          - expected
          + actual
  FAIL
  FAIL    example.com/project/pkg/parser   0.235s
  ```
  For panics: preserve `panic: ...`, goroutine stack, `testing.tRunner`, and package `FAIL` line.
- **Prevalence**: 5/5 in Go repos; common for AFT bash users even outside JS/TS.
- **Existing shared logic**: No direct compressor. Cargo test logic is conceptually similar (test failure sections + final result), but markers differ: `--- FAIL:`, indented file diagnostics, package `FAIL` rows.
- **Truncation risk**: High, especially `-v` and `./...`: passing package/test output can surround failures; final package summary lacks `t.Errorf` messages, diffs, and panic stacks.
- **Recommendation**: Top non-JS addition.

### 3. Jest
- **CLI**: `jest`, `npx jest`, `pnpm jest`, `npm test -- --runInBand` when script invokes Jest.
- **Failure block to preserve**:
  ```text
  FAIL  src/foo.test.ts
    Foo
      ✕ returns value (12 ms)

    expect(received).toEqual(expected) // deep equality

    - Expected  - 1
    + Received  + 1

    - "ok"
    + "bad"

      10 | test('returns value', () => {
      11 |   const result = foo()
    > 12 |   expect(result).toEqual('ok')
         |                  ^
  ```
  Also preserve `Snapshot Summary`, `Received has value`, and stack frames under the `FAIL` suite.
- **Prevalence**: 5/5 in JS/TS repos.
- **Existing shared logic**: Mostly yes. `vitest.rs` already matches command tokens `vitest | jest`, parses Jest JSON, and text `FAIL`/`PASS`/`Test Suites:`/`Tests:` summaries. Gap: commands hidden behind `npm test`, `pnpm test`, `bun run test`, `yarn test` may be captured by package-manager compressors before Vitest/Jest matching unless script command is visible.
- **Truncation risk**: High. Final `Test Suites:`/`Tests:` summary omits matcher diffs and code frames.
- **Recommendation**: Ensure dispatcher/package-manager paths actually route Jest text to Vitest/Jest compressor; may need shared test-runner detection rather than a new parser.

### 4. Deno test
- **CLI**: `deno test`, `deno test -A`, `deno task test` when it invokes `deno test`.
- **Failure block to preserve**:
  ```text
  ERRORS

  test name ... FAILED (5ms)

  AssertionError: Values are not equal.

  [Diff] Actual / Expected

  - actual
  + expected

      throw new AssertionError(message);
            ^
      at assertEquals (.../asserts.ts:190:9)
      at file:///repo/foo_test.ts:12:3

  FAILURES

  test name => ./foo_test.ts:10:6

  FAILED | 12 passed | 1 failed (3s)
  ```
- **Prevalence**: 3/5 overall JS/TS; 4/5 in Deno-specific repos.
- **Existing shared logic**: No. Similar to pytest section-header compression (`ERRORS`, `FAILURES`) but Deno block syntax and final `FAILED | passed | failed` summary differ.
- **Truncation risk**: High. Summary preserves counts only; assertion diffs/stacks are in the mid-run `ERRORS`/`FAILURES` sections.
- **Recommendation**: Good standalone compressor or reusable section parser.

### 5. cargo-nextest
- **CLI**: `cargo nextest run`, `cargo nextest run --workspace`.
- **Failure block to preserve**:
  ```text
  FAIL [   0.012s] crate::module test_name
  stdout ───
  ... captured stdout ...
  stderr ───
  thread 'test_name' panicked at src/lib.rs:42:5:
  assertion `left == right` failed
    left: 1
   right: 2
  stack backtrace:
  ...

  Summary [   1.234s] 100 tests run: 99 passed, 1 failed
  ```
- **Prevalence**: 3/5 in Rust repos; very common in larger/CI-oriented Rust workspaces.
- **Existing shared logic**: Partially. `cargo.rs` only matches head `cargo` and subcommands `build|check|clippy|test`; `cargo nextest run` currently falls through generic. Cargo test block logic cannot directly parse nextest `FAIL [time]` plus `stdout/stderr ───` sections.
- **Truncation risk**: High in large workspaces; final `Summary` lacks captured stdout/stderr and panic/assertion context.
- **Recommendation**: Add `cargo nextest` branch before generic cargo fallback.

### 6. AVA
- **CLI**: `ava`, `npx ava`, `pnpm ava`.
- **Failure block to preserve**:
  ```text
    ✘ [fail]: file › macro › test title

    Error: expected true to be false

    › test.ts:12:5

    11: const value = run()
    12: t.false(value)
            ^

    Difference (- actual, + expected):
    - true
    + false
  ```
- **Prevalence**: 2/5 today; still present in many Node libraries.
- **Existing shared logic**: No. Could share JS assertion/diff preservation concepts with Vitest/Jest but markers are `✘`, `✔`, `›` and `n tests failed`.
- **Truncation risk**: High. Summary counts do not include assertion messages/code frames.
- **Recommendation**: Medium priority due to lower prevalence.

### 7. Mocha
- **CLI**: `mocha`, `npx mocha`, `npm test` when script invokes Mocha.
- **Failure block to preserve**:
  ```text
    1) Array
         #indexOf()
           should return -1 when not present:

       AssertionError: expected 0 to equal -1
       + expected - actual

       -0
       +-1
       at Context.<anonymous> (test/array.spec.js:8:12)
  ```
- **Prevalence**: 4/5 historically; 3/5 in new JS/TS repos.
- **Existing shared logic**: No. Distinct numbered failure list after pass dots/spec output. Vitest/Jest `FAIL` markers do not apply.
- **Truncation risk**: Medium-high. In default spec/dot reporters, detailed numbered failures usually appear near the end before summary; large stdout or many tests can still push them into the dropped middle. Summary only says `N failing`.
- **Recommendation**: Worth supporting after Bun/Jest/Go/Deno/nextest.

### 8. node:test built-in runner
- **CLI**: `node --test`, `node --test test/**/*.test.js`.
- **Failure block to preserve**:
  ```text
  not ok 3 - returns value
    ---
    duration_ms: 1.23
    location: '/repo/test/foo.test.js:10:1'
    failureType: 'testCodeFailure'
    error: 'Expected values to be strictly equal:'
    code: 'ERR_ASSERTION'
    expected: 1
    actual: 2
    operator: 'strictEqual'
    stack: |-
      TestContext.<anonymous> (/repo/test/foo.test.js:12:10)
    ...
  ```
- **Prevalence**: 3/5 and rising in Node 18+/20+ projects.
- **Existing shared logic**: No. TAP/YAML-ish format; could share with TAP parsers.
- **Truncation risk**: High. Final TAP plan/summary omits error object and stack.
- **Recommendation**: Consider with TAP support.

### 9. TAP ecosystem
- **CLI**: `tap`, `npx tap`, `pnpm tap`, sometimes `node --test` TAP output.
- **Failure block to preserve**:
  ```text
  not ok 12 - should parse config
    ---
    at:
      line: 42
      column: 5
      file: test/config.ts
    found: null
    wanted: object
    compare: ===
    stack: |-
      Test.<anonymous> (test/config.ts:42:5)
    ...
  ```
- **Prevalence**: 2/5 broad JS; higher in older Node/npm-package ecosystems.
- **Existing shared logic**: No. Can share with node:test TAP/YAML preservation.
- **Truncation risk**: High. Final `# failed N`/plan lacks diagnostics.
- **Recommendation**: Lower prevalence but simple markers (`not ok` + YAML diagnostic).

### 10. Swift test
- **CLI**: `swift test`, `swift test --parallel`.
- **Failure block to preserve**:
  ```text
  Test Case '-[PackageTests.FooTests testBar]' failed (0.001 seconds)
  /repo/Tests/FooTests/FooTests.swift:12: error: FooTests.testBar : XCTAssertEqual failed: ("1") is not equal to ("2") -
  Test Suite 'FooTests' failed at 2026-05-22 12:00:00.000.
       Executed 10 tests, with 1 failure (0 unexpected) in 0.123 seconds
  ```
  Also compile diagnostics from SwiftPM before test execution.
- **Prevalence**: 2/5 overall; 5/5 in Swift repos.
- **Existing shared logic**: No. Similar conceptually to xUnit line preservation.
- **Truncation risk**: Medium-high for large verbose suites; final suite summary lacks assertion expression/details if failure lines are dropped.
- **Recommendation**: Niche but valuable for Swift workspaces.

## Candidates already covered or lower priority

### pytest-xdist
- **CLI**: `pytest -n auto`, `python -m pytest -n 4`.
- **Failure block**:
  ```text
  gw0 [100] / gw1 [100]
  tests/test_api.py::test_user FAILED                                      [ 50%]
  =================================== FAILURES ===================================
  _________________________________ test_user __________________________________
  [gw0] darwin -- Python 3.12.0 /venv/bin/python
  Traceback / assertion diff / captured stdout
  =========================== short test summary info ===========================
  FAILED tests/test_api.py::test_user - AssertionError: ...
  ```
- **Prevalence**: 3/5 in Python projects with larger suites.
- **Existing shared logic**: Yes, existing `pytest.rs` should preserve `FAILURES`, `ERRORS`, warnings, short summary. Need only validate xdist worker prefix lines (`[gw0]`) and progress noise.
- **Truncation risk**: High in raw output, but likely already mitigated.

### ESLint flat config
- **CLI**: `eslint .`, `npx eslint .` regardless of `.eslintrc` vs `eslint.config.js`.
- **Failure block**:
  ```text
  /repo/src/foo.ts
    12:7  error  'x' is assigned a value but never used  no-unused-vars
    13:1  warning  Missing return type                  @typescript-eslint/explicit-function-return-type

  ✖ 2 problems (1 error, 1 warning)
  ```
- **Prevalence**: 5/5 JS/TS.
- **Existing shared logic**: Yes, existing `eslint.rs` should cover; flat-config changes configuration loading, not output format.
- **Truncation risk**: High in raw output but already covered.

### tsc --pretty / TypeScript compile errors
- **CLI**: `tsc --noEmit --pretty`, `npx tsc -p tsconfig.json --pretty`.
- **Failure block**:
  ```text
  src/foo.ts:12:7 - error TS2322: Type 'string' is not assignable to type 'number'.

  12 const n: number = "x";
           ~

  Found 1 error in src/foo.ts:12
  ```
- **Prevalence**: 5/5 TS.
- **Existing shared logic**: Yes, existing `tsc.rs` is intended for this exact family.
- **Truncation risk**: High in raw output, but already covered.

### rustc compile errors via cargo build/check/test
- **CLI**: `cargo check`, `cargo build`, `cargo test`; direct `rustc` is rare.
- **Failure block**:
  ```text
  error[E0308]: mismatched types
    --> src/lib.rs:12:18
     |
  12 |     let x: i32 = "no";
     |            ---   ^^^^ expected `i32`, found `&str`
     |
  error: could not compile `crate` (lib) due to 1 previous error
  ```
- **Prevalence**: 5/5 Rust via cargo; 1/5 direct rustc.
- **Existing shared logic**: Cargo path is covered by `cargo.rs` for `build|check|clippy`. Direct `rustc` has no compressor but is uncommon.
- **Truncation risk**: High for raw cargo/rustc diagnostics; cargo covered.

### mypy
- **CLI**: `mypy .`, `python -m mypy .`.
- **Failure block**:
  ```text
  pkg/foo.py:12: error: Incompatible return value type (got "str", expected "int")  [return-value]
  pkg/foo.py:13: note: Revealed type is "builtins.str"
  Found 1 error in 1 file (checked 200 source files)
  ```
- **Prevalence**: 4/5 in typed Python repos; 2/5 all Python.
- **Existing shared logic**: No dedicated module, but line-oriented output is already dense.
- **Truncation risk**: Medium. The final summary does not include details, but diagnostics are one/few lines each and not separated by large pass output. A simple TOML filter may be enough; no rich multi-line blocks usually hidden in the middle.

### pyright CLI
- **CLI**: `pyright`, `npx pyright`, `pnpm pyright`.
- **Failure block**:
  ```text
  /repo/pkg/foo.py
    /repo/pkg/foo.py:12:9 - error: Type "str" is not assignable to declared type "int" (reportAssignmentType)
    /repo/pkg/foo.py:13:13 - information: Type of "x" is "str"
  1 error, 0 warnings, 1 information
  ```
- **Prevalence**: 3/5 typed Python.
- **Existing shared logic**: No dedicated module; output is concise and grouped by file.
- **Truncation risk**: Medium. Summary loses detail, but output generally lacks pass noise; generic truncation only hurts very large error sets.

### ruff lint / format
- **CLI**: `ruff check .`, `ruff format --check .`.
- **Failure block**:
  ```text
  path/to/file.py:12:5: F841 Local variable `x` is assigned to but never used
     |
  10 | def f():
  11 |     x = 1
     |     ^ F841
  12 |     return 2
     |
  Found 1 error.
  ```
- **Prevalence**: 5/5 new Python repos.
- **Existing shared logic**: No; could be TOML/rust line-diagnostic parser. Not a test runner.
- **Truncation risk**: Medium. Rich code frames can be lost in very large lint runs, but there is no pass list before a mid-run failure; all diagnostics are the main output.

### golangci-lint
- **CLI**: `golangci-lint run ./...`.
- **Failure block**:
  ```text
  pkg/foo.go:12:7: ineffectual assignment to x (ineffassign)
      x := 1
      ^
  pkg/bar.go:20:1: File is not `gofmt`-ed with `gofmt` (gofmt)
  ```
- **Prevalence**: 4/5 in mature Go repos.
- **Existing shared logic**: No; line-oriented lint diagnostics.
- **Truncation risk**: Medium. Final summary may be absent or only counts; diagnostics are all output, not hidden between pass list and summary.

### oxlint
- **CLI**: `oxlint .`, `npx oxlint .`.
- **Failure block**:
  ```text
  warning[eslint/no-unused-vars]: 'foo' is assigned a value but never used
    --> src/foo.ts:12:7
     |
  12 | const foo = 1
     |       ^^^
  ```
- **Prevalence**: 2/5 but rising in JS/TS monorepos.
- **Existing shared logic**: Possibly could share diagnostic/code-frame logic with Biome/ESLint, but output format is rustc-like.
- **Truncation risk**: Medium. Rich code frames can be lost, but no pass-list/mid-summary shape.

### ktlint
- **CLI**: `ktlint`, `ktlint "**/*.kt"`, Gradle wrappers often `./gradlew ktlintCheck`.
- **Failure block**:
  ```text
  /repo/src/main/kotlin/Foo.kt:12:1: Needless blank line(s)
  /repo/src/main/kotlin/Foo.kt:13:5: Missing newline before ")"
  Summary error count (descending) by rule:
    standard:no-consecutive-blank-lines: 1
  ```
- **Prevalence**: 2/5 overall; 4/5 in Kotlin repos with ktlint.
- **Existing shared logic**: No; line-oriented.
- **Truncation risk**: Low-medium. Summary has counts by rule, not locations; diagnostics are concise and not surrounded by pass output.

### Prettier --check
- **CLI**: `prettier --check .`, `npx prettier --check .`.
- **Failure block**:
  ```text
  Checking formatting...
  [warn] src/foo.ts
  [warn] src/bar.ts
  [warn] Code style issues found in 2 files. Run Prettier with --write to fix.
  ```
- **Prevalence**: 5/5 JS/TS.
- **Existing shared logic**: Could be simple TOML filter; no Rust parser needed.
- **Truncation risk**: Low. There is no rich failure detail beyond filenames; summary already states what happened. Preserve first/last warning filenames if needed.

## Not recommended as custom Rust compressors right now

- **`mypy`, `pyright`, `ruff`, `golangci-lint`, `oxlint`, `ktlint`**: useful to compress eventually, but they mostly emit diagnostics as the main body rather than pass-noise -> failure-block -> summary. TOML filters or a shared generic compiler/linter diagnostic compressor may be enough.
- **`prettier --check`**: failure detail is only file names; no rich hidden block.
- **Direct `rustc`**: rich diagnostics but uncommon compared with cargo paths already covered.

## Top 5 implementation recommendations

1. `bun test` — immediate trigger, high JS/TS prevalence, not covered.
2. `go test ./...` — very common, rich `--- FAIL`/panic blocks, not covered.
3. Jest behind package-manager scripts — parser exists, but ensure `npm|pnpm|bun run test` does not bypass it.
4. `deno test` — rich `ERRORS`/`FAILURES` sections, no current coverage.
5. `cargo nextest run` — common Rust CI runner, not covered by existing cargo test compressor.
