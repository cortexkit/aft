use aft::compress::cargo::CargoCompressor;
use aft::compress::generic::{dedup_consecutive, middle_truncate, strip_ansi};
use aft::compress::git::GitCompressor;
use aft::compress::{self, Compressor};
use aft::config::Config;
use aft::context::AppContext;
use aft::parser::TreeSitterProvider;

fn compress_context(enabled: bool) -> AppContext {
    AppContext::new(
        Box::new(TreeSitterProvider::new()),
        Config {
            experimental_bash_compress: enabled,
            ..Config::default()
        },
    )
}

#[test]
fn generic_strips_ansi_escape_sequences() {
    assert_eq!(
        strip_ansi("plain \x1b[31mred\x1b[0m text"),
        "plain red text"
    );
}

#[test]
fn generic_dedups_consecutive_lines() {
    let input = "same\nsame\nsame\nsame\nsame\n";
    assert_eq!(dedup_consecutive(input), "same\n... (4 more)\n");
}

#[test]
fn generic_middle_truncate_respects_threshold() {
    let small = "x".repeat(9 * 1024);
    assert_eq!(middle_truncate(&small, 10 * 1024, 10, 10), small);

    let large = format!(
        "{}{}{}",
        "a".repeat(6 * 1024),
        "middle",
        "z".repeat(6 * 1024)
    );
    let compressed = middle_truncate(&large, 10 * 1024, 128, 128);
    assert!(compressed.starts_with(&"a".repeat(128)));
    assert!(compressed.contains("...<truncated "));
    assert!(compressed.ends_with(&"z".repeat(128)));
}

#[test]
fn git_status_groups_large_sections() {
    let mut output = String::from("On branch main\nChanges not staged for commit:\n");
    for index in 0..50 {
        output.push_str(&format!("\tmodified:   src/file_{index}.rs\n"));
    }

    let compressed = GitCompressor.compress("git status", &output);
    assert!(compressed.contains("Changes not staged for commit:"));
    assert!(compressed.contains("src/file_0.rs"));
    assert!(compressed.contains("... and 40 more"));
    assert!(!compressed.contains("src/file_49.rs"));
}

#[test]
fn git_diff_preserves_file_boundaries() {
    let mut output = String::new();
    for file in 0..7 {
        output.push_str(&format!(
            "diff --git a/src/file_{file}.rs b/src/file_{file}.rs\n--- a/src/file_{file}.rs\n+++ b/src/file_{file}.rs\n"
        ));
        for hunk in 0..4 {
            output.push_str(&format!("@@ -{hunk},1 +{hunk},1 @@\n"));
            for line in 0..40 {
                output.push_str(&format!("+added {file} {hunk} {line}\n"));
            }
        }
    }

    let compressed = GitCompressor.compress("git diff", &output);
    assert!(compressed.contains("diff --git a/src/file_0.rs b/src/file_0.rs"));
    assert!(compressed.contains("--- a/src/file_0.rs"));
    assert!(compressed.contains("+++ b/src/file_0.rs"));
    assert!(compressed.contains("... +"));
    assert!(compressed.contains("... and 2 more files changed"));
}

#[test]
fn cargo_build_keeps_warnings_and_drops_compiling_lines() {
    let output = "   Updating crates.io index\n   Compiling dep v1.2.3\n   Compiling app v0.1.0\nwarning: unused variable: `x`\n  --> src/lib.rs:1:9\n   |\n1  | let x = 1;\n   |     ^\n\n    Finished dev [unoptimized + debuginfo] target(s) in 1.23s\n";

    let compressed = CargoCompressor.compress("cargo build", output);
    assert!(compressed.contains("warning: unused variable"));
    assert!(compressed.contains("--> src/lib.rs:1:9"));
    assert!(compressed.contains("Finished dev"));
    assert!(!compressed.contains("Compiling dep"));
    assert!(!compressed.contains("Updating crates.io index"));
}

#[test]
fn cargo_test_preserves_failures_verbatim() {
    let output = "running 2 tests\ntest ok_test ... ok\ntest failing_test ... FAILED\n\nfailures:\n\n---- failing_test stdout ----\nthread 'failing_test' panicked at src/lib.rs:2:5:\nboom\nnote: run with `RUST_BACKTRACE=1` environment variable to display a backtrace\n\nfailures:\n    failing_test\n\ntest result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s\n";

    let compressed = CargoCompressor.compress("cargo test", output);
    assert!(compressed.contains("running 2 tests"));
    assert!(!compressed.contains("test ok_test ... ok"));
    assert!(compressed.contains("---- failing_test stdout ----"));
    assert!(compressed.contains("boom"));
    assert!(compressed.contains("test result: FAILED"));
}

#[test]
fn dispatch_falls_back_to_generic_for_unknown_commands() {
    let ctx = compress_context(true);
    let output = "\x1b[31mred\x1b[0m\n".to_string();
    assert_eq!(compress::compress("unknown", output, &ctx), "red\n");
}

#[test]
fn dispatch_returns_unchanged_output_when_disabled() {
    let ctx = compress_context(false);
    let output = "\x1b[31mred\x1b[0m\n".to_string();
    assert_eq!(compress::compress("unknown", output.clone(), &ctx), output);
}
