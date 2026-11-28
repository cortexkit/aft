use crate::bash_background::output::cap_completion_output;
use crate::compress::caps::{DropClass, COMPLETION_FAILURE_TAIL_BYTES as FAILURE_DETAIL_BYTES};
use crate::compress::cargo::{
    failure_summary_preserved, FAILURE_DETAIL_LINES, FAILURE_SECTION_LINES,
};
use crate::compress::generic::GenericCompressor;
use crate::compress::{
    builtin_filters, compress_with_registry_exit_code, toml_filter::build_registry, Compressor,
};

const HOST_SUPERVISOR_FAILURES: &str = include_str!(
    "../../aft/tests/integration/fixtures/compress_filters/cargo-test/host-supervisor-two-failures.stdout"
);

#[test]
fn cargo_test_specimen_panic_messages_survive_completion_preview() {
    let registry = build_registry(builtin_filters::ALL, None, None);
    let result = compress_with_registry_exit_code(
        "cargo test --locked -p thalamus-harness-module --features keeper-test-seams --test host_supervisor",
        HOST_SUPERVISOR_FAILURES,
        Some(101),
        &registry,
    );
    let preview = cap_completion_output(&result.text, false);
    assert_eq!(result.input_line_count, 367);
    assert!(result.text.len() <= FAILURE_DETAIL_BYTES, "{}", result.text);
    assert!(!preview.truncated, "{}", preview.text);
    for message in [
        "the log kept 64000 bytes, 64 of the host's 100 filler lines",
        "the host 6851 still runs 4.5s after its keeper's death",
    ] {
        assert!(result.text.contains(message), "{}", result.text);
        assert!(preview.text.contains(message), "{}", preview.text);
    }
    for noise in [
        "Compiling ",
        "Finished `test`",
        "Running tests/",
        "warning:",
        "RUST_BACKTRACE",
        " ... FAILED",
    ] {
        assert!(!result.text.contains(noise), "{}", result.text);
    }
    assert_eq!(result.text.matches(" stdout ----").count(), 2);
    assert!(result.text.contains("failures:\n    holders::"));
    assert!(result
        .text
        .contains("test result: FAILED. 2 passed; 2 failed"));
    assert_eq!(result.dropped_by_class.get(&DropClass::Warning), Some(&8));

    // Before bounded cargo summaries, the shared failure guard treated the
    // word `passed` in a failing verdict as success and returned generic text.
    let before = GenericCompressor.compress_with_exit_code(
        "cargo test",
        HOST_SUPERVISOR_FAILURES,
        Some(101),
    );
    println!(
        "BEFORE COMPRESSED OUTPUT:\n{}\nAFTER COMPRESSED OUTPUT:\n{}",
        before.text, result.text
    );
}

#[test]
fn cargo_test_preserves_same_failure_name_from_two_binaries() {
    let binary = "running 1 test\ntest tests::foo ... FAILED\n\nfailures:\n\n---- tests::foo stdout ----\nthread 'tests::foo' panicked at src/lib.rs:1:1:\nassertion failed\n\nfailures:\n    tests::foo\n\ntest result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out\n";
    let raw = format!(
        "{}{binary}{binary}error: test failed, to rerun pass `--lib`\n",
        "   Compiling fixture v0.1.0\n".repeat(80)
    );
    let registry = build_registry(builtin_filters::ALL, None, None);
    let result = compress_with_registry_exit_code("cargo test", &raw, Some(101), &registry);
    assert!(result.bounded_cargo_test_details);
    assert_eq!(
        result
            .text
            .lines()
            .filter(|line| *line == "    tests::foo")
            .count(),
        2,
        "{}",
        result.text
    );
    assert!(failure_summary_preserved(&raw, &result.text));
    let collapsed = result.text.replacen("    tests::foo\n", "", 1).replacen(
        "---- tests::foo stdout ----\n",
        "",
        1,
    );
    assert!(!failure_summary_preserved(&raw, &collapsed));
    let preview = cap_completion_output(&result.text, false);
    assert_eq!(
        preview
            .text
            .lines()
            .filter(|line| *line == "    tests::foo")
            .count(),
        2
    );
}

#[test]
fn cargo_test_many_failures_keep_first_message_after_preview_cap() {
    let mut output = String::from("running 30 tests\nfailures:\n");
    for index in 0..30 {
        output.push_str(&format!("---- case_{index} stdout ----\nnoise before panic\nthread 'case_{index}' panicked at src/lib.rs:{index}:1:\nmessage for failure {index}\nnote: run with `RUST_BACKTRACE=1` environment variable to display a backtrace\n{}", "extra message context\n".repeat(50)));
    }
    output.push_str("failures:\n");
    for index in 0..30 {
        output.push_str(&format!("    case_{index}\n"));
    }
    output.push_str(
        "test result: FAILED. 0 passed; 30 failed; 0 ignored; 0 measured; 0 filtered out\n",
    );
    let registry = build_registry(builtin_filters::ALL, None, None);
    let result = compress_with_registry_exit_code("cargo test", &output, Some(101), &registry);
    let (details, names) = result.text.split_once("failures:\n").expect("failure list");
    assert!(details.lines().count() <= FAILURE_DETAIL_LINES + 1);
    assert!(result.text.len() <= FAILURE_DETAIL_BYTES);
    assert!(!result.text.contains("RUST_BACKTRACE"));
    assert!(!result.text.contains("noise before panic"));
    for section in details.split("---- ").skip(1) {
        assert!(section.lines().count() <= FAILURE_SECTION_LINES);
    }
    let preview = cap_completion_output(&result.text, false);
    assert!(preview.text.contains("message for failure 0"));
    for index in 0..30 {
        let expected = format!("    case_{index}");
        assert!(names.lines().any(|line| line == expected));
        assert!(preview.text.lines().any(|line| line == expected));
    }
}
