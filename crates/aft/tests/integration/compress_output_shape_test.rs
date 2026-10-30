use aft::compress::biome::BiomeCompressor;
use aft::compress::builtin_filters::ALL;
use aft::compress::bun::BunCompressor;
use aft::compress::eslint::EslintCompressor;
use aft::compress::mypy::MypyCompressor;
use aft::compress::toml_filter::{build_registry, FilterRegistry};
use aft::compress::tsc::TscCompressor;
use aft::compress::vitest::VitestCompressor;
use aft::compress::{self, Compressor};

fn empty_registry() -> FilterRegistry {
    FilterRegistry::default()
}

fn compress(command: &str, output: &str) -> String {
    compress::compress_with_registry(command, output, &empty_registry()).text
}

fn compress_with_builtin_filters(command: &str, output: &str) -> String {
    let registry = build_registry(ALL, None, None);
    compress::compress_with_registry(command, output, &registry).text
}

fn bun_failure_output(summary: &str) -> String {
    format!(
        "bun test v1.3.14\n\nsrc/foo.test.ts:\n(pass) passes [0.1ms]\nerror: expect(received).toBe(expected)\n  10 | expect(value).toBe(2)\n(fail) fails [0.2ms]\n\n 1 pass\n 1 fail\n 2 expect() calls\n{summary}\n"
    )
}

#[test]
fn bun_output_shape_does_not_match_arbitrary_text_with_ran_line() {
    let output = "custom runner starting\n(pass) copied status text without timing\nRan 2 tests across 1 file. [1.00ms]\ncustom runner done\n";

    assert!(!BunCompressor.matches_output(output));
    assert_eq!(compress("./scripts/custom-runner", output), output);
}

#[test]
fn bun_output_shape_matches_real_bun_test_output() {
    let output = bun_failure_output("Ran 2 tests across 1 file. [1.00ms]");

    assert!(BunCompressor.matches_output(&output));

    let compressed = compress("npm test", &output);

    assert!(compressed.contains("(fail) fails [0.2ms]"));
    // Ran-summary kept, its [Xms] duration stripped.
    assert!(compressed.contains("Ran 2 tests across 1 file."));
    assert!(!compressed.contains("[1.00ms]"));
    assert!(!compressed.contains("(pass) passes"));
}

#[test]
fn bun_run_cwd_test_routes_by_bun_test_output_shape() {
    let output = bun_failure_output("Ran 2 tests across 1 file. [1.00ms]");

    let compressed = compress("bun run --cwd packages/foo test", &output);

    assert!(compressed.contains("(fail) fails [0.2ms]"));
    assert!(compressed.contains("Ran 2 tests across 1 file."));
    assert!(!compressed.contains("[1.00ms]"));
    assert!(!compressed.contains("(pass) passes"));
}

#[test]
fn bun_run_test_routes_by_bun_test_output_shape_plural_summary() {
    let output = bun_failure_output("Ran 5 tests across 3 files. [2.00ms]");

    let compressed = compress("bun run test", &output);

    assert!(compressed.contains("(fail) fails [0.2ms]"));
    assert!(compressed.contains("Ran 5 tests across 3 files."));
    assert!(!compressed.contains("[2.00ms]"));
    assert!(!compressed.contains("(pass) passes"));
}

#[test]
fn npm_test_with_bun_output_routes_to_bun_not_npm_generic() {
    let output = bun_failure_output("Ran 2 tests across 1 file. [1.00ms]");

    let compressed = compress("npm test", &output);

    assert!(compressed.contains("error: expect(received).toBe(expected)"));
    assert!(compressed.contains("(fail) fails [0.2ms]"));
    assert!(!compressed.contains("(pass) passes"));
}

#[test]
fn npm_test_with_vitest_output_routes_to_vitest() {
    let output = "RERUN  src/foo.test.ts x1\nFAIL src/foo.test.ts\n  Expected: 1\n  Received: 2\nPASS src/bar.test.ts\nTest Files  1 failed | 1 passed (2)\nTests       1 failed | 1 passed (2)\nDuration    1.26s\n";

    let compressed = compress("npm test", output);

    assert!(compressed.contains("FAIL src/foo.test.ts"));
    assert!(compressed.contains("PASS src/bar.test.ts"));
    assert!(compressed.contains("Duration    1.26s"));
    assert!(!compressed.contains("RERUN"));
}

#[test]
fn npm_test_with_jest_output_routes_to_vitest_jest_branch() {
    let output = "....\nPASS src/foo.test.js\nTest Suites: 1 passed, 1 total\nTests:       3 passed, 3 total\n";

    let compressed = compress("npm test", output);

    assert!(compressed.contains("PASS src/foo.test.js"));
    assert!(compressed.contains("Test Suites: 1 passed, 1 total"));
    assert!(compressed.contains("Tests:       3 passed, 3 total"));
    assert!(!compressed.contains("...."));
}

#[test]
fn pnpm_test_with_pytest_output_routes_to_pytest() {
    let output = "============================= test session starts =============================\nplatform darwin -- Python\ncollected 2 items\n\n....F\n\n=================================== FAILURES ===================================\n___________________________________ test_bar ___________________________________\nE   AssertionError: boom\n=========================== short test summary info ===========================\nFAILED tests/test_foo.py::test_bar - AssertionError\n========================= 1 failed, 1 passed in 0.12s =========================\n";

    let compressed = compress("pnpm test", output);

    assert!(compressed.contains("FAILURES"));
    assert!(compressed.contains("1 failed, 1 passed in 0.12s"));
    assert!(!compressed.contains("....F"));
}

#[test]
fn yarn_test_with_playwright_output_routes_to_playwright() {
    let output = "Running 2 tests using 1 worker\n  ✓  1 [chromium] › a.spec.ts:1:1 › passes (1s)\n  ✓  2 [chromium] › b.spec.ts:1:1 › passes (2s)\n\n  2 passed (3.2s)\n";

    let compressed = compress("yarn test", output);

    assert_eq!(compressed, "playwright: 2 tests passed (3.2s)");
}

#[test]
fn npm_run_lint_with_eslint_output_routes_to_eslint() {
    let output = "/repo/src/foo.js\n  1:10  error    'foo' is defined but never used  no-unused-vars\n\n✖ 1 problem (1 error, 0 warnings)\n";

    let compressed = compress("npm run lint", output);

    assert!(compressed.contains("/repo/src/foo.js"));
    assert!(compressed.contains("1:10 error no-unused-vars 'foo' is defined but never used"));
    assert!(compressed.contains("✖ 1 problem (1 error, 0 warnings)"));
}

#[test]
fn npm_run_lint_with_biome_output_routes_to_biome() {
    let output = "Checked 1 file in 2ms\nsrc/main.ts:1:1\nlint/suspicious/noConsole ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n\n  ✖ Don't use console.\n\nFound 1 error.\n";

    let compressed = compress("npm run lint", output);

    assert!(compressed.contains("src/main.ts:1:1"));
    assert!(compressed.contains("lint/suspicious/noConsole"));
    assert!(compressed.contains("Found 1 error."));
    assert!(!compressed.contains("Checked 1 file in 2ms"));
}

#[test]
fn npm_run_typecheck_with_tsc_output_routes_to_tsc() {
    let output = "Starting typecheck...\nsrc/index.ts(1,7): error TS2322: Type 'string' is not assignable to type 'number'.\nFound 1 error in 1 file\n";

    let compressed = compress("npm run typecheck", output);

    assert_eq!(
        compressed,
        "src/index.ts(1,7): error TS2322: Type 'string' is not assignable to type 'number'."
    );
}

#[test]
fn make_test_with_cargo_output_routes_to_cargo_before_make_filter() {
    // Forty passing tests put the fixture past the cargo compressor's 1 KiB raw
    // passthrough, below which output is returned as printed.
    let passing = (0..40)
        .map(|index| format!("test ok_test_{index} ... ok\n"))
        .collect::<String>();
    let output = format!("running 42 tests\ntest ok_test ... ok\n{passing}test failing_test ... FAILED\n\nfailures:\n\n---- failing_test stdout ----\nboom\n\ntest result: FAILED. 41 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out\n");
    assert!(output.len() > 1024);

    let compressed = compress_with_builtin_filters("make test", &output);

    assert!(compressed.contains("running 42 tests"));
    assert!(compressed.contains("---- failing_test stdout ----"));
    assert!(compressed.contains("test result: FAILED"));
    assert!(!compressed.contains("test ok_test ... ok"));
}

#[test]
fn just_test_with_go_output_routes_to_go() {
    let output = "=== RUN   TestPass\n--- PASS: TestPass (0.00s)\n=== RUN   TestFail\n--- FAIL: TestFail (0.01s)\n    foo_test.go:10: got false\nFAIL\nFAIL\texample.com/pkg\t0.100s\n";

    let compressed = compress("just test", output);

    assert!(compressed.contains("--- FAIL: TestFail (0.01s)"));
    assert!(compressed.contains("foo_test.go:10: got false"));
    assert!(compressed.contains("FAIL\texample.com/pkg\t0.100s"));
    assert!(!compressed.contains("TestPass"));
    assert!(!compressed.contains("=== RUN"));
}

#[test]
fn shell_script_with_ruff_output_routes_to_ruff() {
    let output = "Checking Python files...\nsrc/a.py:10:5: E501 Line too long (88 > 79 characters)\nFound 1 error.\n";

    let compressed = compress("./scripts/test.sh", output);

    assert!(compressed.contains("src/a.py:10:5: E501 Line too long"));
    assert!(compressed.contains("Found 1 error."));
    assert!(!compressed.contains("Checking Python files"));
}

#[test]
fn shell_script_with_mypy_output_routes_to_mypy() {
    let output = "src/a.py:1: note: Standalone note\nsrc/a.py:10: error: Incompatible types in assignment  [assignment]\nFound 1 error in 1 file (checked 2 source files)\n";

    let compressed = compress("./scripts/check.sh", output);

    assert!(compressed.contains("src/a.py:10: error: Incompatible types"));
    assert!(compressed.contains("Found 1 error in 1 file"));
    assert!(!compressed.contains("Standalone note"));
}

#[test]
fn make_lint_with_golangci_lint_output_routes_to_golangci_lint() {
    let output = "level=info msg=running linters\nsrc/foo.go:10:5: unused variable `x` (unused)\n1 issues:\n* unused: 1\n";

    let compressed = compress("make lint", output);

    assert!(compressed.contains("src/foo.go:10:5: unused variable `x` (unused)"));
    assert!(compressed.contains("1 issues:"));
    assert!(!compressed.contains("level=info"));
}

#[test]
fn npm_run_format_check_with_prettier_output_routes_to_prettier() {
    let output = "Checking formatting...\n[warn] src/a.ts\n[warn] Code style issues found in 1 file. Run Prettier with --write to fix.\n";

    let compressed = compress("npm run format:check", output);

    assert!(compressed.contains("[warn] src/a.ts"));
    assert!(compressed.contains("Code style issues found in 1 file"));
    assert!(!compressed.contains("Checking formatting"));
}

#[test]
fn package_manager_install_commands_remain_command_only() {
    let npm_output = "npm http fetch GET 200 https://registry.npmjs.org/foo 123ms\nnpm WARN deprecated old-pkg@1.0.0: use new-pkg instead\nadded 42 packages in 2s\naudited 100 packages in 2s\nfound 0 vulnerabilities\n";
    let npm = compress("npm install", npm_output);
    assert!(npm.contains("audited 100 packages"));
    assert!(!npm.contains("npm http fetch"));

    let pnpm_output = "Progress: resolved 1, downloaded 1, added 1\nProgress: resolved 2, downloaded 2, added 2\nProgress: resolved 3, downloaded 3, added 3\ndependencies:\n+ left-pad 1.3.0\nDone in 1s\n";
    let pnpm = compress("pnpm install", pnpm_output);
    assert!(pnpm.contains("Progress: resolved 1"));
    assert!(pnpm.contains("Progress: resolved 2"));
    assert!(!pnpm.contains("Progress: resolved 3"));
    assert!(pnpm.contains("Done in 1s"));

    let bun_output = "Resolving dependencies\nSaved lockfile\n1 package installed\n";
    let bun = compress("bun install", bun_output);
    assert!(bun.contains("Saved lockfile"));
    assert!(bun.contains("1 package installed"));
    assert!(!bun.contains("Resolving dependencies"));
}

#[test]
fn cargo_and_go_build_commands_do_not_get_pulled_into_test_output_sniffers() {
    // The dependency `Compiling` lines put the fixture past the cargo
    // compressor's 1 KiB raw passthrough, below which output is returned as
    // printed.
    let compiling = (0..40)
        .map(|index| format!("   Compiling dep{index} v1.2.3\n"))
        .collect::<String>();
    let cargo_output = format!("{compiling}   Compiling app v0.1.0\nPASS\nwarning: unused variable: `x`\n --> src/lib.rs:1:9\n    Finished `dev` profile [unoptimized] target(s) in 0.12s\n");
    assert!(cargo_output.len() > 1024);
    let cargo = compress("cargo build", &cargo_output);
    assert!(cargo.contains("warning: unused variable"));
    assert!(cargo.contains("Finished `dev` profile"));
    // Only the cargo build summary drops cargo's own `Compiling` lines.
    assert!(!cargo.contains("Compiling app"));
    // A line cargo did not write is not the summary's to drop.
    assert!(cargo.contains("PASS\nwarning: unused variable"));

    let go_output = "PASS\nmain.go:10:5: undefined: missingFunc\n";
    let go = compress("go build ./...", go_output);
    assert_eq!(go, "main.go:10:5: undefined: missingFunc");
}

#[test]
fn output_signature_edge_cases_do_not_overclaim() {
    assert!(!BunCompressor.matches_output("Ran 1 tests across 1 file. [0.50ms]\n"));
    assert!(!BunCompressor.matches_output("Ran 5 tests across 3 files. [1.50ms]\n"));
    assert!(!BunCompressor.matches_output(
        "(pass) copied status text without timing\nRan 1 tests across 1 file. [0.50ms]\n"
    ));

    let vitest_partial = "RERUN src/foo.test.ts x1\nTest Files  1 passed (1)\n";
    assert!(!VitestCompressor.matches_output(vitest_partial));
    assert!(compress("npm test", vitest_partial).contains("RERUN"));

    let tsc_summary_only = "Found 3 errors in 2 files.\n";
    assert!(!TscCompressor.matches_output(tsc_summary_only));
    assert_eq!(
        compress("npm run typecheck", tsc_summary_only),
        tsc_summary_only.trim_end()
    );

    assert!(!EslintCompressor.matches_output("[]"));
    assert_eq!(compress("npm run lint", "[]"), "[]");

    assert!(!BiomeCompressor.matches_output("parse/syntax ━━━━━━━━━━\n"));
}

#[test]
fn multiple_output_signatures_use_deterministic_registry_order() {
    let output = "src/index.ts(1,1): error TS2322: Type 'string' is not assignable to type 'number'.\nFound 1 error in 1 file\n";

    assert!(TscCompressor.matches_output(output));
    assert!(MypyCompressor.matches_output(output));

    let compressed = compress("./scripts/check.sh", output);

    assert_eq!(
        compressed,
        "src/index.ts(1,1): error TS2322: Type 'string' is not assignable to type 'number'."
    );
}
