use std::fs;
use std::path::PathBuf;

use aft::compress::builtin_filters::ALL;
use aft::compress::toml_filter::{apply_filter, build_registry, parse_filter, FilterSource};

fn fixture_dir(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/integration/fixtures/compress_filters")
        .join(name)
}

fn load_filter(name: &str) -> aft::compress::toml_filter::TomlFilter {
    let (_, content) = ALL
        .iter()
        .find(|(n, _)| *n == name)
        .unwrap_or_else(|| panic!("builtin filter {name} not registered"));
    parse_filter(name, content, FilterSource::Builtin).expect("parse builtin")
}

fn run_fixture(name: &str) {
    let dir = fixture_dir(name);
    let input = fs::read_to_string(dir.join("input.txt")).expect("input.txt");
    let expected = fs::read_to_string(dir.join("expected.txt")).expect("expected.txt");
    let filter = load_filter(name);
    let actual = apply_filter(&filter, &input);
    assert_eq!(
        actual.trim_end(),
        expected.trim_end(),
        "fixture mismatch for {name}",
    );
}

#[test]
fn ansible_playbook_filter() {
    run_fixture("ansible-playbook");
}

#[test]
fn df_filter() {
    run_fixture("df");
}

#[test]
fn docker_filter() {
    run_fixture("docker");
}

#[test]
fn du_filter() {
    run_fixture("du");
}

#[test]
fn find_filter() {
    run_fixture("find");
}

#[test]
fn gh_filter() {
    run_fixture("gh");
}

#[test]
fn gradle_filter() {
    run_fixture("gradle");
}

#[test]
fn helm_filter() {
    run_fixture("helm");
}

#[test]
fn kubectl_filter() {
    run_fixture("kubectl");
}

#[test]
fn ls_filter() {
    run_fixture("ls");
}

#[test]
fn terraform_filter() {
    run_fixture("terraform");
}

#[test]
fn tree_filter() {
    run_fixture("tree");
}

#[test]
fn wc_filter() {
    run_fixture("wc");
}

#[test]
fn xcodebuild_filter() {
    run_fixture("xcodebuild");
}

#[test]
fn make_shortcircuit_only_matches_empty_body() {
    let filter = load_filter("make");

    assert_eq!(apply_filter(&filter, ""), "make: ok");

    let with_inner_blank = apply_filter(&filter, "error\n\nhint");
    assert_eq!(with_inner_blank, "error\n\nhint");
}

#[test]
fn toml_filter_strip_ansi_false_sees_raw_ansi() {
    let registry = build_registry(
        &[(
            "ansi-raw",
            r#"
[filter]
matches = ["ansi-raw"]

[strip]
patterns = []

[shortcircuit]
when = "\\x1b\\[31m"
replacement = "matched raw ansi"

[ansi]
strip = false
"#,
        )],
        None,
        None,
    );

    let actual =
        aft::compress::compress_with_registry("ansi-raw", "\u{1b}[31mred\u{1b}[0m", &registry);

    assert_eq!(actual, "matched raw ansi");
}

fn compress_builtin(command: &str, output: &str) -> String {
    let registry = build_registry(ALL, None, None);
    aft::compress::compress_with_registry(command, output, &registry)
}

#[test]
fn deno_check_shortcircuits_clean_output() {
    let output = "Check file:///tmp/main.ts OK";
    let compressed = compress_builtin("deno check main.ts", output);

    assert_eq!(compressed, "deno check: ok");
}

#[test]
fn pip_install_already_satisfied_shortcircuits() {
    let output = "Requirement already satisfied: numpy in /usr/lib/python3.10\nRequirement already satisfied: pandas in /usr/lib/python3.10";
    let compressed = compress_builtin("pip install numpy pandas", output);

    assert_eq!(compressed, "pip: already satisfied");
}

#[test]
fn uv_audit_shortcircuits_clean_output() {
    let output = "Audited 42 packages";
    let compressed = compress_builtin("uv pip install -r requirements.txt", output);

    assert_eq!(compressed, "uv: audited packages");
}

#[test]
fn aws_filter_strips_initialization_and_truncates_long_lines() {
    let long_value = "x".repeat(800);
    let output = format!("Initializing AWS CLI ...\n{{\"Value\":\"{long_value}\"}}");
    let compressed = compress_builtin("aws ec2 describe-instances", &output);

    assert!(!compressed.contains("Initializing AWS CLI"));
    assert!(compressed.contains("Value"));
    assert!(compressed.len() < output.len());
}

#[test]
fn psql_empty_result_shortcircuits() {
    let output = "+----+\n+----+\n(0 rows)";
    let compressed = compress_builtin("psql -c 'select * from t'", output);

    assert_eq!(compressed, "psql: (0 rows)");
}

#[test]
fn curl_filter_strips_progress_and_verbose_connection_noise() {
    let output = "  % Total    % Received % Xferd  Average Speed   Time    Time     Time  Current\n* Trying 127.0.0.1:443...\n* Connected to example.com (127.0.0.1) port 443\n{\"ok\":true}";
    let compressed = compress_builtin("curl -v https://example.com", output);

    assert!(!compressed.contains("% Total"));
    assert!(!compressed.contains("* Trying"));
    assert!(!compressed.contains("* Connected"));
    assert!(compressed.contains("{\"ok\":true}"));
    assert!(compressed.len() < output.len());
}

#[test]
fn wget_successful_download_shortcircuits() {
    let output = "Resolving example.com (example.com)... 93.184.216.34\nConnecting to example.com (example.com)|93.184.216.34|:443... connected.\nHTTP request sent, awaiting response... 200 OK\n'index.html' saved [1234/1234]";
    let compressed = compress_builtin("wget https://example.com", output);

    assert_eq!(compressed, "wget: downloaded");
}
