use aft::compress::bun::BunCompressor;
use aft::compress::npm::NpmCompressor;
use aft::compress::pnpm::PnpmCompressor;
use aft::compress::pytest::PytestCompressor;
use aft::compress::tsc::TscCompressor;
use aft::compress::{self, Compressor};
use aft::config::Config;
use aft::context::AppContext;
use aft::parser::TreeSitterProvider;

fn compress_context() -> AppContext {
    AppContext::new(
        Box::new(TreeSitterProvider::new()),
        Config {
            experimental_bash_compress: true,
            ..Config::default()
        },
    )
}

#[test]
fn npm_install_caps_deprecations_and_keeps_summary() {
    let mut output = String::new();
    for index in 0..8 {
        output.push_str(&format!(
            "npm WARN deprecated package-{index}@1.0.0: use replacement-{index}\n"
        ));
        output.push_str(&format!(
            "npm http fetch GET 200 https://registry.npmjs.org/package-{index} 12ms\n"
        ));
    }
    output.push_str("added 300 packages in 10s\n\n80 packages are looking for funding\n  run `npm fund` for details\n\naudited 301 packages in 11s\nfound 0 vulnerabilities\n");

    let compressed = NpmCompressor.compress("npm install", &output);
    assert!(compressed.contains("package-0@1.0.0"));
    assert!(compressed.contains("package-4@1.0.0"));
    assert!(compressed.contains("... and 3 more deprecation warnings"));
    assert!(!compressed.contains("package-7@1.0.0"));
    assert!(!compressed.contains("npm http fetch"));
    assert!(!compressed.contains("added 300 packages"));
    assert!(compressed.contains("audited 301 packages"));
    assert!(compressed.contains("found 0 vulnerabilities"));

    let ratio = compressed.len() as f32 / output.len() as f32;
    assert!(ratio < 0.70, "ratio was {ratio}");
}

#[test]
fn bun_install_drops_resolver_noise_but_keeps_errors_and_summary() {
    let mut output = String::new();
    for index in 0..30 {
        output.push_str(&format!("Resolving dependencies {index}/30\n"));
        output.push_str(&format!("Downloaded dep-{index}\n"));
    }
    output.push_str("error: GET https://registry.example/dep - 500\n42 packages installed [1234.00ms]\nSaved lockfile\n");

    let compressed = BunCompressor.compress("bun install", &output);
    assert!(!compressed.contains("Resolving dependencies"));
    assert!(!compressed.contains("Downloaded dep-"));
    assert!(compressed.contains("error: GET https://registry.example/dep - 500"));
    assert!(compressed.contains("42 packages installed"));
    assert!(compressed.contains("Saved lockfile"));

    let ratio = compressed.len() as f32 / output.len() as f32;
    assert!(ratio < 0.15, "ratio was {ratio}");
}

#[test]
fn pnpm_install_limits_progress_and_keeps_auth_warning_error_summary() {
    let mut output =
        String::from("Lockfile is up to date\nAlready up-to-date\nAlready up-to-date\n");
    for index in 0..12 {
        output.push_str(&format!(
            "Progress: resolved {}, reused {}, downloaded {}, added {}\n",
            index * 10,
            index,
            index + 1,
            index + 2
        ));
    }
    output.push_str("WARN GET_NO_AUTH 401 https://registry.example/private\nERR_PNPM_FETCH_401 No authorization header was set\ndependencies:\n+ react 18.2.0\n- left-pad 1.3.0\nDone in 4.2s\n");

    let compressed = PnpmCompressor.compress("pnpm install", &output);
    assert_eq!(compressed.matches("Progress: resolved").count(), 2);
    assert_eq!(compressed.matches("Already up-to-date").count(), 1);
    assert!(compressed.contains("WARN GET_NO_AUTH"));
    assert!(compressed.contains("ERR_PNPM_FETCH_401"));
    assert!(compressed.contains("dependencies:"));
    assert!(compressed.contains("Done in 4.2s"));

    let ratio = compressed.len() as f32 / output.len() as f32;
    assert!(ratio < 0.45, "ratio was {ratio}");
}

#[test]
fn pytest_drops_passes_keeps_failures_summary_and_warning_cap() {
    let mut output = String::from("============================= test session starts =============================\nplatform darwin -- Python 3.12.1, pytest-8.1.1\nrootdir: /repo\ncollected 45 items\n\ntests/test_ok.py ............................ PASSED\ntests/test_more.py sssxxx PASSED\ntests/test_bad.py::test_breaks FAILED\n\n=================================== FAILURES ===================================\n______________________________ test_breaks ______________________________\nE   AssertionError: boom\n\n=============================== warnings summary ===============================\n");
    for index in 0..8 {
        output.push_str(&format!(
            "tests/test_warn.py:{index}: DeprecationWarning: deprecated api {index}\n"
        ));
    }
    output.push_str("=========================== short test summary info ===========================\nFAILED tests/test_bad.py::test_breaks - AssertionError: boom\n==================== 44 passed, 1 failed, 3 skipped in 2.34s ====================\n");

    let compressed = PytestCompressor.compress("python -m pytest", &output);
    assert!(compressed.contains("platform darwin"));
    assert!(compressed.contains("rootdir: /repo"));
    assert!(compressed.contains("collected 45 items"));
    assert!(!compressed.contains("tests/test_ok.py"));
    assert!(compressed.contains("tests/test_bad.py::test_breaks FAILED"));
    assert!(compressed.contains("AssertionError: boom"));
    assert!(compressed.contains("... and 3 more warnings"));
    assert!(compressed.contains("short test summary info"));
    assert!(compressed.contains("44 passed, 1 failed"));

    let ratio = compressed.len() as f32 / output.len() as f32;
    assert!(ratio < 0.80, "ratio was {ratio}");
}

#[test]
fn tsc_groups_errors_by_file_and_handles_clean_output() {
    let mut output = String::from(
        "Project 'tsconfig.json' is out of date because output is older than input\nCompiling...\n",
    );
    for index in 0..35 {
        output.push_str(&format!(
            "src/big.ts({},{}): error TS2322: Type 'string' is not assignable to type 'number'.\n",
            index + 1,
            index + 2
        ));
    }
    for file in 0..22 {
        output.push_str(&format!(
            "src/file_{file}.ts(1,1): error TS2304: Cannot find name 'missing{file}'.\n"
        ));
    }
    output.push_str("Found 57 errors in 23 files.\n");

    let compressed = TscCompressor.compress("pnpm tsc --noEmit", &output);
    assert!(!compressed.contains("Compiling..."));
    assert!(compressed.contains("src/big.ts(1,2): error TS2322"));
    assert!(compressed.contains("... and 25 more errors in this file"));
    assert!(compressed.contains("... and 13 more files with errors"));
    assert!(compressed.contains("Found 57 errors in 23 files"));

    let ratio = compressed.len() as f32 / output.len() as f32;
    assert!(ratio < 0.45, "ratio was {ratio}");
}

#[test]
fn tsc_preserves_top_level_errors_and_only_reports_proven_success() {
    let top_level_error = "error TS18003: No inputs were found in config file 'tsconfig.json'. Specified 'include' paths were '[\"src\"]'.\n";
    let compressed = TscCompressor.compress("tsc --noEmit", top_level_error);
    assert!(compressed.contains("error TS18003: No inputs were found"));
    assert!(!compressed.contains("No errors"));

    let watch_success = TscCompressor.compress(
        "tsc --watch",
        "12:00:00 PM - Found 0 errors. Watching for file changes.\n",
    );
    assert_eq!(watch_success, "No errors. [cmpaft]");

    let empty = TscCompressor.compress("tsc --noEmit", "");
    assert_eq!(empty, "No errors. [cmpaft]");
}

#[test]
fn dispatch_reaches_extra_compressors() {
    let ctx = compress_context();
    let output = "Progress: resolved 1, reused 0, downloaded 0, added 0\nProgress: resolved 2, reused 0, downloaded 0, added 0\nProgress: resolved 3, reused 0, downloaded 0, added 0\ndependencies:\n+ zod 3.22.0\n".to_string();

    let compressed = compress::compress("pnpm install", output, &ctx);
    assert_eq!(compressed.matches("Progress: resolved").count(), 2);
    assert!(compressed.contains("dependencies:"));
}

// ---------------------------------------------------------------------------
// `bun test` compressor tests
//
// Regression: until v0.28.2, `bun test` fell through to GenericCompressor,
// which middle-truncates large outputs. Bun emits failure blocks BETWEEN the
// header and the summary, so truncation would routinely lose the error
// detail an agent needs to debug. The new compress_test() preserves all
// failure blocks (capped at MAX_FAILURES) plus header + summary.
// ---------------------------------------------------------------------------

#[test]
fn bun_test_pass_only_keeps_header_and_summary() {
    let output = "bun test v1.3.14 (0d9b296a)\n\nsrc/__tests__/foo.test.ts:\n\n 12 pass\n 0 fail\n 24 expect() calls\nRan 12 tests across 1 file. [42.00ms]\n";

    let compressed = BunCompressor.compress("bun test", output);
    assert!(compressed.contains("bun test v1.3.14"));
    assert!(compressed.contains("12 pass"));
    assert!(compressed.contains("0 fail"));
    assert!(compressed.contains("Ran 12 tests across 1 file. [42.00ms]"));
}

#[test]
fn bun_test_preserves_single_failure_block_when_middle_truncation_would_hit() {
    // Simulate the realistic shape: header + (many) pass-quiet sections +
    // failure block + summary. Bun's default reporter doesn't print
    // individual pass lines, but it does print a section header per file,
    // so the truncation hazard is real for monorepos with many test files.
    let mut output = String::from("bun test v1.3.14 (0d9b296a)\n");
    for index in 0..50 {
        output.push_str(&format!("\nsrc/pass_only_{index}.test.ts:\n"));
    }
    output.push_str("\nsrc/failing.test.ts:\n");
    output.push_str("11 | test(\"failing example\", () => {\n");
    output.push_str("12 |   expect({ a: 1 }).toEqual({ a: 2 });\n");
    output.push_str("                          ^\n");
    output.push_str("error: expect(received).toEqual(expected)\n");
    output.push_str("\n@@ -1,3 +1,3 @@\n");
    output.push_str("   {\n");
    output.push_str("-    \"a\": 2,\n");
    output.push_str("+    \"a\": 1,\n");
    output.push_str("   }\n");
    output.push_str("\n      at <anonymous> (/repo/src/failing.test.ts:12:24)\n");
    output.push_str("(fail) failing example [3.43ms]\n");
    output.push_str(
        "\n 49 pass\n 1 fail\n 50 expect() calls\nRan 50 tests across 50 files. [142.00ms]\n",
    );

    let compressed = BunCompressor.compress("bun test", &output);

    // Must preserve the failure block.
    assert!(compressed.contains("error: expect(received).toEqual(expected)"));
    assert!(compressed.contains("(fail) failing example"));
    assert!(compressed.contains("expect({ a: 1 }).toEqual({ a: 2 });"));
    assert!(compressed.contains("at <anonymous>"));
    // Must preserve the file section header that owns the failure.
    assert!(compressed.contains("src/failing.test.ts:"));
    // Must preserve the summary tail.
    assert!(compressed.contains("1 fail"));
    assert!(compressed.contains("Ran 50 tests across 50 files. [142.00ms]"));

    // Pass-only section headers should be dropped (no failure beneath them).
    assert!(!compressed.contains("src/pass_only_0.test.ts:"));
    assert!(!compressed.contains("src/pass_only_49.test.ts:"));

    let ratio = compressed.len() as f32 / output.len() as f32;
    assert!(ratio < 0.50, "ratio was {ratio}");
}

#[test]
fn bun_test_multiple_failures_all_preserved_under_cap() {
    let mut output = String::from("bun test v1.3.14 (0d9b296a)\n\nsrc/multi.test.ts:\n\n");
    for index in 0..3 {
        output.push_str(&format!(
            "{} | expect({}).toBe(0);\n",
            10 + index,
            index + 1
        ));
        output.push_str("              ^\n");
        output.push_str(&format!(
            "error: expect(received).toBe(expected) [{index}]\n"
        ));
        output.push_str("\nExpected: 0\n");
        output.push_str(&format!("Received: {}\n", index + 1));
        output.push_str(&format!(
            "      at <anonymous> (/repo/src/multi.test.ts:{}:5)\n",
            10 + index
        ));
        output.push_str(&format!("(fail) case {index} [0.5ms]\n"));
    }
    output
        .push_str("\n 0 pass\n 3 fail\n 3 expect() calls\nRan 3 tests across 1 file. [12.00ms]\n");

    let compressed = BunCompressor.compress("bun test", &output);

    for index in 0..3 {
        assert!(
            compressed.contains(&format!("expect(received).toBe(expected) [{index}]")),
            "missing failure {index} body in: {compressed}"
        );
        assert!(
            compressed.contains(&format!("(fail) case {index}")),
            "missing (fail) marker {index}"
        );
    }
    assert!(compressed.contains("3 fail"));
    assert!(compressed.contains("Ran 3 tests across 1 file. [12.00ms]"));
    assert!(!compressed.contains("+0 more failures"));
}

#[test]
fn bun_test_catastrophic_failure_count_is_capped() {
    let mut output = String::from("bun test v1.3.14 (0d9b296a)\n\nsrc/disaster.test.ts:\n\n");
    let total = 100usize;
    for index in 0..total {
        output.push_str(&format!(
            "{} | expect({}).toBe(0);\n",
            10 + index,
            index + 1
        ));
        output.push_str("              ^\n");
        output.push_str(&format!("error: failure_marker_{index}\n"));
        output.push_str(&format!("(fail) case_{index} [0.5ms]\n"));
    }
    output.push_str(&format!(
        "\n 0 pass\n {total} fail\n {total} expect() calls\nRan {total} tests across 1 file. [12.00ms]\n"
    ));

    let compressed = BunCompressor.compress("bun test", &output);

    // First 25 failures must be preserved (MAX_FAILURES = 25).
    for index in 0..25 {
        assert!(
            compressed.contains(&format!("failure_marker_{index}")),
            "missing kept failure {index}"
        );
    }
    // Failures past 25 must be dropped from the body.
    for index in 25..total {
        assert!(
            !compressed.contains(&format!("failure_marker_{index}")),
            "did not drop failure {index}"
        );
    }
    // Drop trailer must report the count of dropped failures.
    assert!(
        compressed.contains(&format!("+{} more failures", total - 25)),
        "missing dropped-failures trailer in: {compressed}"
    );
    // Summary intact.
    assert!(compressed.contains(&format!("{total} fail")));
    assert!(compressed.contains(&format!("Ran {total} tests across 1 file. [12.00ms]")));
}

#[test]
fn bun_test_dispatch_routes_through_test_compressor_not_generic() {
    // End-to-end: confirm the registry dispatches `bun test` through the
    // new compress_test path. Without the fix, this output would go to
    // GenericCompressor::compress_output() which preserves all lines and
    // does not skip individual file-section headers; with the fix we drop
    // the pass-only sections and keep the failure block.
    let ctx = compress_context();
    let output = "bun test v1.3.14 (0d9b296a)\n\nsrc/a.test.ts:\n\nsrc/b.test.ts:\n\nsrc/c.test.ts:\n12 | expect(1).toBe(2);\n              ^\nerror: expect(received).toBe(expected)\n(fail) c case [0.5ms]\n\n 0 pass\n 1 fail\n 1 expect() calls\nRan 1 tests across 3 files. [3.00ms]\n".to_string();

    let compressed = compress::compress("bun test", output, &ctx);
    // Pass-only sections dropped.
    assert!(!compressed.contains("src/a.test.ts:"));
    assert!(!compressed.contains("src/b.test.ts:"));
    // Failing section header preserved.
    assert!(compressed.contains("src/c.test.ts:"));
    // Failure body preserved.
    assert!(compressed.contains("error: expect(received).toBe(expected)"));
    assert!(compressed.contains("(fail) c case"));
    // Summary tail preserved.
    assert!(compressed.contains("1 fail"));
    assert!(compressed.contains("Ran 1 tests across 3 files. [3.00ms]"));
}

// ---------------------------------------------------------------------------
// Chained-command output preservation (v0.29 dogfood)
//
// Real DB sweep showed agents frequently invoke `bun test` chained with
// other commands: `bun test && bun run build`, `pwd && git status && bun
// test`, `bun run typecheck && bun run lint && bun test && bun run build`,
// etc. Before these tests, `compress_test` aggressively stripped lines
// outside the bun-test block, silently losing any chained-command output
// that came after `Ran N tests across M files. [Xms]`. The fix preserves
// trailing lines unchanged so agents see all the chain's output.
// ---------------------------------------------------------------------------

#[test]
fn bun_test_pass_only_preserves_chained_command_output() {
    // `bun test && echo done; ls -la dist/` — bun test passes, chained
    // commands run, their output appears AFTER the `Ran ...` summary.
    let output = "bun test v1.3.14 (0d9b296a)\n\n 12 pass\n 0 fail\n 24 expect() calls\nRan 12 tests across 1 file. [42.00ms]\ndone\ntotal 16\n-rw-r--r--  1 user  staff  4096 May 22 19:00 bundle.js\n-rw-r--r--  1 user  staff   512 May 22 19:00 styles.css\n";

    let compressed = BunCompressor.compress("bun test", output);
    // bun test header + summary preserved as before
    assert!(compressed.contains("bun test v1.3.14"));
    assert!(compressed.contains("12 pass"));
    assert!(compressed.contains("Ran 12 tests across 1 file. [42.00ms]"));
    // Chained command output (echo, ls) must survive
    assert!(
        compressed.contains("done"),
        "lost chained `echo done` output"
    );
    assert!(
        compressed.contains("bundle.js"),
        "lost chained `ls -la dist/` output"
    );
    assert!(compressed.contains("styles.css"), "lost chained ls listing");
}

#[test]
fn bun_test_with_failures_preserves_chained_command_output() {
    // `bun test; echo "always runs"` — bun test fails, but `;` separator
    // (unlike `&&`) means the chained command still runs. Failure block
    // AND chained output both preserved.
    let output = "bun test v1.3.14 (0d9b296a)\n\nsrc/foo.test.ts:\n11 | expect(x).toBe(y)\nerror: expect(received).toBe(expected)\n(fail) foo case [1.00ms]\n\n 0 pass\n 1 fail\n 1 expect() calls\nRan 1 tests across 1 files. [42.00ms]\nalways runs\n";

    let compressed = BunCompressor.compress("bun test", output);
    // failure block preserved
    assert!(compressed.contains("error: expect(received).toBe(expected)"));
    assert!(compressed.contains("(fail) foo case"));
    // summary preserved
    assert!(compressed.contains("Ran 1 tests across 1 files. [42.00ms]"));
    // Chained command output after `Ran ...` preserved
    assert!(
        compressed.contains("always runs"),
        "lost chained command output that runs after `bun test` (with `;` separator)"
    );
}
