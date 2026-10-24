use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use filetime::{set_file_mtime, FileTime};
use serde_json::{json, Value};

use super::helpers::{link_cached_path, user_config, warm_executable, AftProcess};

fn setup_project(files: &[(&str, &str)]) -> tempfile::TempDir {
    let temp_dir = tempfile::tempdir().expect("create temp dir");

    // Ignore-rule fixtures need a real repository: a partial .git directory is
    // corrupt metadata, not evidence that this project is outside Git.
    if files.iter().any(|(path, _)| path.starts_with(".git/")) {
        let output = std::process::Command::new("git")
            .args(["init", "--quiet"])
            .arg(temp_dir.path())
            .output()
            .expect("initialize fixture repository");
        assert!(output.status.success(), "{output:?}");
    }

    for (relative_path, content) in files {
        let path = temp_dir.path().join(relative_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent directories");
        }
        fs::write(path, content).expect("write fixture file");
    }

    temp_dir
}

/// Configure with the trigram index switched off, so grep and glob exercise
/// the direct-scan fallback path. The index defaults on, so the fallback cases
/// must turn it off explicitly.
fn configure(aft: &mut AftProcess, root: &Path) {
    let resp = send(
        aft,
        json!({
            "id": "cfg-no-index",
            "command": "configure",
            "harness": "opencode",
            "project_root": root,
            "config": user_config(serde_json::json!({ "indexes": { "trigram": false } }))
        }),
    );
    assert_eq!(resp["success"], true, "configure should succeed: {resp:?}");
}

fn configure_with_index(aft: &mut AftProcess, root: &Path) {
    let resp = send(
        aft,
        json!({
            "id": "cfg-index",
            "command": "configure",
            "harness": "opencode",
            "project_root": root,
            "config": user_config(serde_json::json!({ "indexes": { "trigram": true } }))
        }),
    );
    assert_eq!(resp["success"], true, "configure should succeed: {resp:?}");
}

fn send(aft: &mut AftProcess, request: Value) -> Value {
    aft.send(&serde_json::to_string(&request).expect("serialize request"))
}

fn fake_server_path() -> PathBuf {
    crate::test_helpers::fake_lsp::fake_server_binary()
}

fn wait_for_index_ready<F>(aft: &mut AftProcess, mut request: F) -> Value
where
    F: FnMut() -> Value,
{
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut last_response = None;

    while Instant::now() < deadline {
        let response = send(aft, request());
        if response["index_status"] == "Ready" {
            return response;
        }

        last_response = Some(response);
        thread::sleep(Duration::from_millis(50));
    }

    panic!(
        "search index should become ready within 10s; last response: {:?}",
        last_response
    );
}

#[test]
fn grep_rejects_invalid_regex_with_pattern_data() {
    let project = setup_project(&[("src/main.rs", "fn main() {}\n")]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let response = send(
        &mut aft,
        json!({
            "id": "grep-invalid-regex",
            "command": "grep",
            "pattern": "[",
        }),
    );

    assert_eq!(response["success"], false, "grep should fail: {response:?}");
    assert_eq!(response["code"], "invalid_pattern");
    assert_eq!(response["pattern"], "[");
    assert!(response["message"]
        .as_str()
        .expect("message")
        .contains("invalid regex"));

    let status = aft.shutdown();
    assert!(status.success());
}

fn canonical_path_string(path: &Path) -> String {
    fs::canonicalize(path)
        .expect("canonicalize path")
        .display()
        .to_string()
}

#[test]
fn glob_fallback_small_tree_has_no_walk_truncated() {
    let project = setup_project(&[("src/a.rs", "fn a() {}\n"), ("src/b.rs", "fn b() {}\n")]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let response = send(
        &mut aft,
        json!({
            "id": "glob-no-walk-trunc",
            "command": "glob",
            "pattern": "src/**/*.rs",
        }),
    );

    assert_eq!(
        response["success"], true,
        "glob should succeed: {response:?}"
    );
    assert_eq!(response["complete"], true);
    assert!(response.get("walk_truncated").is_none() || response["walk_truncated"] == false);

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn glob_returns_newest_files_first_as_tool_contract_promises() {
    let project = setup_project(&[
        ("a-old.txt", "old\n"),
        ("z-new.txt", "new\n"),
        ("m-middle.txt", "middle\n"),
    ]);
    for (name, seconds) in [
        ("a-old.txt", 1_700_000_000),
        ("z-new.txt", 1_700_000_300),
        ("m-middle.txt", 1_700_000_150),
    ] {
        set_file_mtime(
            project.path().join(name),
            FileTime::from_unix_time(seconds, 0),
        )
        .expect("set fixture mtime");
    }

    let expected = ["z-new.txt", "m-middle.txt", "a-old.txt"]
        .map(|name| canonical_path_string(&project.path().join(name)));
    let observed_mtimes = expected
        .iter()
        .map(|path| {
            fs::metadata(path)
                .expect("read fixture metadata")
                .modified()
                .expect("read fixture mtime")
        })
        .collect::<Vec<_>>();
    assert!(
        observed_mtimes.windows(2).all(|pair| pair[0] > pair[1]),
        "fixture mtimes must be strictly newest-first"
    );

    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());
    let response = send(
        &mut aft,
        json!({
            "id": "glob-mtime-order",
            "command": "glob",
            "pattern": "*.txt",
        }),
    );

    assert_eq!(
        response["success"], true,
        "glob should succeed: {response:?}"
    );
    let actual = response["files"]
        .as_array()
        .expect("files array")
        .iter()
        .map(|path| path.as_str().expect("file path"))
        .collect::<Vec<_>>();
    assert_eq!(actual, expected, "glob must return newest files first");
    assert_eq!(
        response["text"], "3 files matching *.txt\n\nz-new.txt\nm-middle.txt\na-old.txt",
        "agent-facing glob text must use the same newest-first order"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

fn rg_available() -> bool {
    std::process::Command::new("rg")
        .arg("--version")
        .output()
        .is_ok()
}

#[test]
fn grep_fallback_returns_relative_paths_and_counts() {
    let project = setup_project(&[
        ("src/one.rs", "fn alpha() { println!(\"alpha\"); }\n"),
        ("src/two.rs", "fn beta() { println!(\"alpha\"); }\n"),
        ("notes.txt", "alpha beta gamma\n"),
    ]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let response = send(
        &mut aft,
        json!({
            "id": "grep-fallback",
            "command": "grep",
            "pattern": r#""alpha""#,
            "include": ["src/**/*.rs"],
        }),
    );

    assert_eq!(
        response["success"], true,
        "grep should succeed: {response:?}"
    );
    assert_eq!(response["index_status"], "Fallback");
    assert_eq!(response["total_matches"], 2);
    assert_eq!(response["files_with_matches"], 2);
    assert_eq!(response["files_searched"], 2);
    assert_eq!(response["complete"], true);
    assert!(response.get("walk_truncated").is_none() || response["walk_truncated"] == false);

    let matches = response["matches"].as_array().expect("matches array");
    assert_eq!(matches.len(), 2);
    assert_eq!(matches[0]["line"], 1);
    assert!(matches[0]["column"].as_u64().unwrap_or(0) >= 1);
    // Files are returned as absolute paths
    let file_path = matches[0]["file"].as_str().expect("file path");
    let file_path = file_path.replace('\\', "/");
    assert!(file_path.contains("src/one.rs") || file_path.contains("src/two.rs"));

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn grep_regex_over_em_dash_does_not_panic() {
    let project = setup_project(&[(
        "src/notes.md",
        "Intro — the em dash here can confuse byte offsets\n",
    )]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let response = send(
        &mut aft,
        json!({
            "id": "grep-em-dash",
            "command": "grep",
            "pattern": "—",
        }),
    );

    assert_eq!(
        response["success"], true,
        "grep should succeed: {response:?}"
    );
    assert!(
        response["total_matches"].as_u64().unwrap_or(0) >= 1,
        "expected at least one match: {response:?}"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn grep_reports_empty_scope_separately() {
    let project = setup_project(&[]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let response = send(
        &mut aft,
        json!({
            "id": "grep-empty-scope",
            "command": "grep",
            "pattern": "anything",
        }),
    );

    assert_eq!(
        response["success"], true,
        "grep should succeed: {response:?}"
    );
    assert_eq!(response["complete"], true);
    assert_eq!(response["no_files_matched_scope"], true);
    assert!(
        response["text"]
            .as_str()
            .expect("text")
            .contains("nothing was searched"),
        "grep text must say an empty scope searched nothing: {response:?}"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

const IGNORE_RULES_NEEDLE: &str = "ignore_rules_needle";

/// A tree whose top-level `.gitignore` hides `vendor/deps/` and `*.log`, and
/// whose nested `src/nested/.gitignore` hides `generated/`. Every file holds
/// the same needle, so grep shows which files a walk or index admitted.
fn ignore_rules_project(git: bool) -> tempfile::TempDir {
    let source = format!("fn marker() {{ {IGNORE_RULES_NEEDLE}(); }}\n");
    let project = setup_project(&[
        (".gitignore", "vendor/deps/\n*.log\n"),
        ("src/nested/.gitignore", "generated/\n"),
        ("src/main.rs", source.as_str()),
        ("src/nested/keep.rs", source.as_str()),
        ("vendor/other.rs", source.as_str()),
        ("vendor/deps/lib.rs", source.as_str()),
        ("vendor/deps/inner/deep.rs", source.as_str()),
        ("src/nested/generated/out.rs", source.as_str()),
        ("trace.log", source.as_str()),
    ]);
    if git {
        let mut command = std::process::Command::new("git");
        crate::test_helpers::apply_hermetic_git_env(command.current_dir(project.path()));
        let status = command.args(["init", "-q"]).status().expect("git init");
        assert!(status.success(), "git init failed");
    }
    project
}

fn relative_files(root: &Path, files: impl IntoIterator<Item = String>) -> Vec<String> {
    let root = fs::canonicalize(root).expect("canonicalize root");
    let mut relative = files
        .into_iter()
        .map(|file| {
            Path::new(&file)
                .strip_prefix(&root)
                .unwrap_or_else(|_| panic!("{file} is outside {}", root.display()))
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect::<Vec<_>>();
    relative.sort();
    relative.dedup();
    relative
}

/// The files grep finds the needle in and the files glob lists, through the
/// trigram index when `indexed`, else through the direct-scan fallback.
fn grep_and_glob_files(root: &Path, indexed: bool) -> (Vec<String>, Vec<String>) {
    let mut aft = AftProcess::spawn();
    let grep_request = || {
        json!({
            "id": "grep-ignore-rules",
            "command": "grep",
            "pattern": IGNORE_RULES_NEEDLE,
        })
    };
    let grep = if indexed {
        configure_with_index(&mut aft, root);
        wait_for_index_ready(&mut aft, grep_request)
    } else {
        configure(&mut aft, root);
        send(&mut aft, grep_request())
    };
    assert_eq!(grep["success"], true, "grep should succeed: {grep:?}");
    let grepped = grep["matches"]
        .as_array()
        .expect("matches array")
        .iter()
        .map(|found| found["file"].as_str().expect("match file").to_string());
    let grepped = relative_files(root, grepped);

    let glob = send(
        &mut aft,
        json!({ "id": "glob-ignore-rules", "command": "glob", "pattern": "**/*" }),
    );
    assert_eq!(glob["success"], true, "glob should succeed: {glob:?}");
    let globbed = glob["files"]
        .as_array()
        .expect("files array")
        .iter()
        .map(|file| file.as_str().expect("glob file").to_string());
    let globbed = relative_files(root, globbed);

    assert!(aft.shutdown().success());
    (grepped, globbed)
}

#[test]
fn grep_and_glob_skip_gitignored_paths_in_non_git_root_like_git_root() {
    let plain = ignore_rules_project(false);
    let git = ignore_rules_project(true);
    let visible = vec![
        "src/main.rs".to_string(),
        "src/nested/keep.rs".to_string(),
        "vendor/other.rs".to_string(),
    ];
    let ignored = [
        "vendor/deps/lib.rs",
        "vendor/deps/inner/deep.rs",
        "src/nested/generated/out.rs",
        "trace.log",
    ];

    for indexed in [true, false] {
        let mode = if indexed { "index" } else { "fallback" };
        let (plain_grep, plain_glob) = grep_and_glob_files(plain.path(), indexed);
        assert_eq!(
            plain_grep, visible,
            "{mode}: non-git grep must find the needle only in files no .gitignore hides"
        );
        for path in ignored {
            assert!(
                !plain_glob.iter().any(|listed| listed == path),
                "{mode}: non-git glob listed gitignored {path}: {plain_glob:?}"
            );
        }

        let (git_grep, git_glob) = grep_and_glob_files(git.path(), indexed);
        assert_eq!(
            plain_grep, git_grep,
            "{mode}: grep differs between non-git and git"
        );
        assert_eq!(
            plain_glob, git_glob,
            "{mode}: glob differs between non-git and git"
        );
    }
}

#[test]
fn glob_fallback_respects_gitignore_and_returns_absolute_paths() {
    let project = setup_project(&[
        ("src/keep.rs", "fn keep() {}\n"),
        ("src/skip.ts", "const skip = true;\n"),
        ("ignored.log", "secret\n"),
        (".gitignore", "*.log\n"),
    ]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let response = send(
        &mut aft,
        json!({
            "id": "glob-fallback",
            "command": "glob",
            "pattern": "src/**/*.rs",
        }),
    );

    assert_eq!(
        response["success"], true,
        "glob should succeed: {response:?}"
    );
    assert_eq!(response["total"], 1);
    let files = response["files"].as_array().expect("files array");
    assert_eq!(
        files,
        &vec![Value::String(canonical_path_string(
            &project.path().join("src/keep.rs")
        ))]
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn glob_reports_empty_scope_separately() {
    let project = setup_project(&[]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let response = send(
        &mut aft,
        json!({
            "id": "glob-empty-scope",
            "command": "glob",
            "pattern": "**/*.rs",
        }),
    );

    assert_eq!(
        response["success"], true,
        "glob should succeed: {response:?}"
    );
    assert_eq!(response["complete"], true);
    assert_eq!(response["no_files_matched_scope"], true);
    assert!(
        response["text"]
            .as_str()
            .expect("text")
            .contains("nothing was searched"),
        "glob text must say an empty scope searched nothing: {response:?}"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn grep_uses_index_when_configured() {
    let project = setup_project(&[
        ("src/search.rs", "fn search() { println!(\"needle\"); }\n"),
        ("src/other.rs", "fn other() { println!(\"haystack\"); }\n"),
    ]);
    let mut aft = AftProcess::spawn();
    configure_with_index(&mut aft, project.path());

    let response = wait_for_index_ready(&mut aft, || {
        json!({
            "id": "grep-indexed",
            "command": "grep",
            "pattern": "needle",
            "include": ["src/**/*.rs"],
        })
    });
    assert_eq!(
        response["success"], true,
        "indexed grep should succeed: {response:?}"
    );
    assert_eq!(response["index_status"], "Ready");
    assert_eq!(response["total_matches"], 1);
    assert_eq!(response["files_with_matches"], 1);
    assert_eq!(response["files_searched"], 1);
    // Files are returned as absolute paths
    let expected_path = canonical_path_string(&project.path().join("src/search.rs"));
    assert_eq!(response["matches"][0]["file"], expected_path);

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn grep_text_uses_relative_paths_and_compact_format() {
    let project = setup_project(&[
        ("src/one.rs", "fn alpha() { println!(\"alpha\"); }\n"),
        ("src/two.rs", "fn beta() { println!(\"alpha\"); }\n"),
    ]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let response = send(
        &mut aft,
        json!({
            "id": "grep-format",
            "command": "grep",
            "pattern": r#""alpha""#,
            "include": ["src/**/*.rs"],
        }),
    );

    assert_eq!(
        response["success"], true,
        "grep should succeed: {response:?}"
    );
    let text = response["text"]
        .as_str()
        .expect("grep text")
        .replace('\\', "/");
    // New format: relative paths, no decorators, line:text format
    assert!(text.contains("src/one.rs\n"));
    assert!(text.contains("src/two.rs\n"));
    // No decorators
    assert!(!text.contains("──"));
    // No "Line" prefix, no indentation
    assert!(text.contains("1: fn alpha()"));
    assert!(text.contains("1: fn beta()"));
    assert!(text.ends_with(
        "Found 2 matches across 2 files [index: fallback; searched all 2 files directly]"
    ));

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn glob_text_uses_relative_paths() {
    let project = setup_project(&[
        ("src/keep.rs", "fn keep() {}\n"),
        ("src/other.rs", "fn other() {}\n"),
    ]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let response = send(
        &mut aft,
        json!({
            "id": "glob-format",
            "command": "glob",
            "pattern": "src/**/*.rs",
        }),
    );

    assert_eq!(
        response["success"], true,
        "glob should succeed: {response:?}"
    );
    let text = response["text"]
        .as_str()
        .expect("glob text")
        .replace('\\', "/");
    // Relative paths in text
    assert!(text.contains("src/keep.rs") || text.contains("src/other.rs"));
    // No absolute paths
    assert!(!text.contains("/private/"));
    assert!(text.starts_with("2 files matching src/**/*.rs"));

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn grep_fallback_supports_line_anchors() {
    let project = setup_project(&[
        (
            "README.md",
            "# Title\n\n## Section One\nbody\n\n## Section Two\nbody\n",
        ),
        (
            "src/lib.rs",
            "// not a heading\n## actually also not\nfn main() {}\n",
        ),
    ]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let response = send(
        &mut aft,
        json!({
            "id": "grep-anchor-fallback",
            "command": "grep",
            "pattern": "^## ",
        }),
    );

    assert_eq!(
        response["success"], true,
        "grep should succeed: {response:?}"
    );
    assert_eq!(response["index_status"], "Fallback");
    // README has two `## ` headings at line start; `src/lib.rs` has one but
    // on a non-first line, so multi_line anchors must match it too.
    assert_eq!(response["total_matches"], 3);
    assert_eq!(response["files_with_matches"], 2);

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn grep_indexed_supports_line_anchors() {
    let project = setup_project(&[
        (".fixture-id", "grep_indexed_supports_line_anchors\n"),
        (
            "README.md",
            "# Title\n\n## Section One\nbody\n\n## Section Two\nbody\n",
        ),
        (
            "src/lib.rs",
            "// not a heading\n## actually also not\nfn main() {}\n",
        ),
    ]);
    let mut aft = AftProcess::spawn();
    configure_with_index(&mut aft, project.path());

    let response = wait_for_index_ready(&mut aft, || {
        json!({
            "id": "grep-anchor-indexed",
            "command": "grep",
            "pattern": "^## ",
        })
    });
    assert_eq!(
        response["success"], true,
        "indexed grep should succeed: {response:?}"
    );
    assert_eq!(response["index_status"], "Ready");
    assert_eq!(response["total_matches"], 3);
    assert_eq!(response["files_with_matches"], 2);

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn case_insensitive_unicode_regex_agrees_between_indexed_and_fallback_grep() {
    let project = setup_project(&[
        (
            ".fixture-id",
            "case_insensitive_unicode_regex_agrees_between_indexed_and_fallback_grep\n",
        ),
        ("release.txt", "\u{212a}ELVIN release\n"),
    ]);
    let request = || {
        json!({
            "id": "grep-case-fold-parity",
            "command": "grep",
            "pattern": "KELVIN.*",
            "case_sensitive": false,
        })
    };

    let mut fallback_aft = AftProcess::spawn();
    configure(&mut fallback_aft, project.path());
    let fallback = send(&mut fallback_aft, request());
    assert_eq!(fallback["index_status"], "Fallback");
    assert_eq!(fallback["total_matches"], 1, "fallback grep: {fallback:?}");
    assert!(fallback_aft.shutdown().success());

    let mut indexed_aft = AftProcess::spawn();
    configure_with_index(&mut indexed_aft, project.path());
    let indexed = wait_for_index_ready(&mut indexed_aft, request);
    assert_eq!(indexed["total_matches"], 1, "indexed grep: {indexed:?}");
    assert!(indexed_aft.shutdown().success());
}

/// Matched `(file name, line, column, match text)` rows of a grep response,
/// sorted so the indexed and walked answers compare independent of order.
fn match_rows(response: &Value) -> Vec<(String, u64, u64, String)> {
    let mut rows: Vec<_> = response["matches"]
        .as_array()
        .expect("matches array")
        .iter()
        .map(|row| {
            let file = row["file"].as_str().expect("file").replace('\\', "/");
            let name = file.rsplit('/').next().unwrap_or_default().to_string();
            (
                name,
                row["line"].as_u64().expect("line"),
                row["column"].as_u64().expect("column"),
                row["match_text"].as_str().expect("match text").to_string(),
            )
        })
        .collect();
    rows.sort();
    rows
}

#[test]
fn em_dash_patterns_match_the_same_lines_on_indexed_and_fallback_grep() {
    let project = setup_project(&[
        (
            ".fixture-id",
            "em_dash_patterns_match_the_same_lines_on_indexed_and_fallback_grep\n",
        ),
        ("docs/notes.md", "Intro — the em dash here\nplain line\n"),
        ("src/lib.rs", "// café — résumé text\nfn main() {}\n"),
    ]);
    // (pattern, case_sensitive, expected matches). The em dash is three UTF-8
    // bytes, so it alone forms a trigram; the others mix it with ASCII and
    // other multi-byte characters, as a literal, a regex and case-folded.
    let cases: [(&str, bool, u64); 4] = [
        ("—", true, 2),
        ("Intro — the", true, 1),
        (r"—\s+r\S+", true, 1),
        ("INTRO — THE", false, 1),
    ];
    let request = |pattern: &str, case_sensitive: bool| {
        json!({
            "id": "grep-em-dash-parity",
            "command": "grep",
            "pattern": pattern,
            "case_sensitive": case_sensitive,
        })
    };

    let mut fallback_aft = AftProcess::spawn();
    configure(&mut fallback_aft, project.path());
    let mut fallback_rows = Vec::new();
    for (pattern, case_sensitive, expected) in cases {
        let fallback = send(&mut fallback_aft, request(pattern, case_sensitive));
        assert_eq!(fallback["index_status"], "Fallback", "{fallback:?}");
        assert_eq!(
            fallback["total_matches"], expected,
            "fallback grep for {pattern:?}: {fallback:?}"
        );
        fallback_rows.push(match_rows(&fallback));
    }
    assert!(fallback_aft.shutdown().success());

    let mut indexed_aft = AftProcess::spawn();
    configure_with_index(&mut indexed_aft, project.path());
    let (first_pattern, first_case_sensitive, _) = cases[0];
    wait_for_index_ready(&mut indexed_aft, || {
        request(first_pattern, first_case_sensitive)
    });
    for ((pattern, case_sensitive, expected), fallback_rows) in cases.into_iter().zip(fallback_rows)
    {
        let indexed = send(&mut indexed_aft, request(pattern, case_sensitive));
        assert_eq!(indexed["index_status"], "Ready", "{indexed:?}");
        assert_eq!(
            indexed["total_matches"], expected,
            "indexed grep for {pattern:?}: {indexed:?}"
        );
        assert_eq!(
            match_rows(&indexed),
            fallback_rows,
            "indexed and fallback grep disagree for {pattern:?}"
        );
    }
    assert!(indexed_aft.shutdown().success());
}

/// While the trigram index is still building, grep answers from a walk. The
/// reply must say the walk read every file, so a zero is trustworthy; a bare
/// "[index: building]" next to the count would leave that unknown. The debug
/// hook holds the first index build open long enough to observe that state.
#[cfg(debug_assertions)]
#[test]
fn grep_while_index_builds_says_the_walk_searched_every_file() {
    let project = setup_project(&[
        (
            ".fixture-id",
            "grep_while_index_builds_says_the_walk_searched_every_file\n",
        ),
        ("src/a.rs", "fn a() {}\n"),
        ("src/b.rs", "fn b() {}\n"),
    ]);
    let mut aft = AftProcess::spawn_with_env(&[(
        "AFT_TEST_SEARCH_REBUILD_PUBLISH_DELAY_MS",
        std::ffi::OsStr::new("5000"),
    )]);
    configure_with_index(&mut aft, project.path());

    let response = send(
        &mut aft,
        json!({
            "id": "grep-while-building",
            "command": "grep",
            "pattern": "definitely_absent_token",
        }),
    );
    assert_eq!(response["success"], true, "{response:?}");
    assert_eq!(response["index_status"], "Building", "{response:?}");
    let text = response["text"].as_str().expect("grep text");
    // The fixture marker file is searched too, so three files in all.
    assert_eq!(
        text, "Found 0 matches in 0 files [index: building; searched all 3 files directly]",
        "{response:?}"
    );
    assert_eq!(response["complete"], true, "{response:?}");
    assert_eq!(response["files_read_directly"], 3, "{response:?}");

    assert!(aft.shutdown().success());
}

#[test]
fn grep_treats_leading_dash_pattern_as_literal() {
    let project = setup_project(&[("notes.txt", "-foo\nbar\n")]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let response = send(
        &mut aft,
        json!({
            "id": "grep-leading-dash",
            "command": "grep",
            "pattern": "-foo",
            "path": project.path(),
        }),
    );

    assert_eq!(
        response["success"], true,
        "grep should succeed: {response:?}"
    );
    assert_eq!(response["total_matches"], 1, "response: {response:?}");

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn grep_fallback_supports_end_of_line_anchor() {
    let project = setup_project(&[("src/a.rs", "fn foo() {}\nfn bar() {}\nlet x = 1;\n")]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    // Match lines ending with `{}` — requires `$` to act as line-end anchor.
    let response = send(
        &mut aft,
        json!({
            "id": "grep-eol",
            "command": "grep",
            "pattern": r"\{\}$",
        }),
    );

    assert_eq!(
        response["success"], true,
        "grep should succeed: {response:?}"
    );
    assert_eq!(response["total_matches"], 2);

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn grep_explicit_gitignored_file_is_searched() {
    // ripgrep parity: naming a gitignored file explicitly searches it anyway.
    let project = setup_project(&[
        ("captures/log.txt", "needle in a gitignored capture\n"),
        (".gitignore", "captures/\n"),
    ]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let response = send(
        &mut aft,
        json!({
            "id": "grep-explicit-gitignored",
            "command": "grep",
            "pattern": "needle",
            "path": "captures/log.txt",
        }),
    );

    assert_eq!(
        response["success"], true,
        "grep should succeed: {response:?}"
    );
    assert_eq!(
        response["total_matches"], 1,
        "explicitly-named gitignored file must be searched: {response:?}"
    );
    assert_eq!(
        response["no_files_matched_scope"], false,
        "explicit existing file must not report empty scope: {response:?}"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn grep_aftignore_excludes_from_directory_search() {
    // .aftignore excludes paths from AFT's directory walk, layered on .gitignore.
    let project = setup_project(&[
        ("keep.rs", "findme here\n"),
        ("vendored/sub.rs", "findme in vendored\n"),
        (".aftignore", "vendored/\n"),
    ]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let response = send(
        &mut aft,
        json!({
            "id": "grep-aftignore-dir",
            "command": "grep",
            "pattern": "findme",
        }),
    );

    assert_eq!(
        response["success"], true,
        "grep should succeed: {response:?}"
    );
    assert_eq!(
        response["total_matches"], 1,
        ".aftignored dir must be excluded from directory search: {response:?}"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn grep_explicit_aftignored_file_is_searched() {
    // Explicitly naming an .aftignored file still searches it (ripgrep parity).
    let project = setup_project(&[
        ("vendored/sub.rs", "needle in aftignored vendored\n"),
        (".aftignore", "vendored/\n"),
    ]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let response = send(
        &mut aft,
        json!({
            "id": "grep-explicit-aftignored",
            "command": "grep",
            "pattern": "needle",
            "path": "vendored/sub.rs",
        }),
    );

    assert_eq!(
        response["success"], true,
        "grep should succeed: {response:?}"
    );
    assert_eq!(
        response["total_matches"], 1,
        "explicitly-named .aftignored file must be searched: {response:?}"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn grep_external_ripgrep_fallback_respects_search_root_aftignore() {
    if !rg_available() {
        return;
    }

    let project = setup_project(&[]);
    let external = setup_project(&[
        ("keep.rs", "external-needle here\n"),
        ("ignored/skip.rs", "external-needle ignored\n"),
        (".aftignore", "ignored/\n"),
    ]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let response = send(
        &mut aft,
        json!({
            "id": "grep-external-rg-aftignore",
            "command": "grep",
            "pattern": "external-needle",
            "path": external.path(),
        }),
    );

    assert_eq!(
        response["success"], true,
        "grep should succeed: {response:?}"
    );
    assert_eq!(response["index_status"], "Fallback");
    assert_eq!(
        response["total_matches"], 1,
        "external ripgrep fallback must honor search-root .aftignore: {response:?}"
    );
    assert_eq!(response["files_with_matches"], 1);
    assert_eq!(
        response["matches"][0]["file"],
        canonical_path_string(&external.path().join("keep.rs"))
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn grep_external_fallback_respects_nested_aftignore() {
    let project = setup_project(&[]);
    let external = setup_project(&[
        ("keep.rs", "nested-external-needle here\n"),
        ("sub/skip.rs", "nested-external-needle ignored\n"),
        ("sub/.aftignore", "skip.rs\n"),
    ]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let response = send(
        &mut aft,
        json!({
            "id": "grep-external-nested-aftignore",
            "command": "grep",
            "pattern": "nested-external-needle",
            "path": external.path(),
        }),
    );

    assert_eq!(
        response["success"], true,
        "grep should succeed: {response:?}"
    );
    assert_eq!(response["index_status"], "Fallback");
    assert_eq!(
        response["total_matches"], 1,
        "external grep must honor nested .aftignore files: {response:?}"
    );
    assert_eq!(
        response["matches"][0]["file"],
        canonical_path_string(&external.path().join("keep.rs"))
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn glob_external_fallback_respects_nested_aftignore() {
    let project = setup_project(&[]);
    let external = setup_project(&[
        ("keep.rs", "fn keep() {}\n"),
        ("sub/skip.rs", "fn skip() {}\n"),
        ("sub/.aftignore", "skip.rs\n"),
    ]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let response = send(
        &mut aft,
        json!({
            "id": "glob-external-nested-aftignore",
            "command": "glob",
            "pattern": "**/*.rs",
            "path": external.path(),
        }),
    );

    assert_eq!(
        response["success"], true,
        "glob should succeed: {response:?}"
    );
    assert_eq!(response["total"], 1, "glob response: {response:?}");
    assert_eq!(
        response["files"].as_array().expect("files"),
        &vec![Value::String(canonical_path_string(
            &external.path().join("keep.rs")
        ))]
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn grep_explicit_file_reports_runtime_fallback_index_status_when_index_disabled() {
    let project = setup_project(&[("src/main.rs", "fn main() { println!(\"needle\"); }\n")]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let response = send(
        &mut aft,
        json!({
            "id": "grep-explicit-index-status",
            "command": "grep",
            "pattern": "needle",
            "path": "src/main.rs",
        }),
    );

    assert_eq!(
        response["success"], true,
        "grep should succeed: {response:?}"
    );
    assert_eq!(response["total_matches"], 1, "response: {response:?}");
    assert_eq!(
        response["index_status"], "Fallback",
        "explicit-file grep must report actual runtime index state, not requested scope indexability: {response:?}"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn semantic_search_with_both_lanes_off_refuses_instead_of_walking() {
    let project = setup_project(&[
        (
            "src/lib.rs",
            "pub fn retry() { /* how retry logic works */ }\n",
        ),
        (
            "ignored/skip.rs",
            "pub fn retry() { /* how retry logic works */ }\n",
        ),
        (".aftignore", "ignored/\n"),
    ]);
    let mut aft = AftProcess::spawn();
    let configure = send(
        &mut aft,
        json!({
            "id": "cfg-semantic-disabled",
            "command": "configure",
            "harness": "opencode",
            "project_root": project.path(),
            "config": user_config(serde_json::json!({
                "indexes": { "trigram": false, "semantic": false }
            })),
        }),
    );
    assert_eq!(
        configure["success"], true,
        "configure should succeed: {configure:?}"
    );

    let response = send(
        &mut aft,
        json!({
            "id": "semantic-degraded-aftignore",
            "command": "semantic_search",
            "query": "how retry logic works",
            "top_k": 10,
        }),
    );

    // With both index lanes configured off aft_search refuses instead of the
    // old degraded filesystem walk, so no file (ignored or not) is returned.
    assert_eq!(response["success"], false, "response: {response:?}");
    assert_eq!(response["code"], "no_search_lanes_enabled");
    assert_eq!(response["results"], serde_json::json!([]));

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn inspect_scoped_diagnostics_respects_aftignore() {
    let fake_server = fake_server_path();
    let project = setup_project(&[
        ("fake.toml", "[project]\n"),
        ("src/main.fake", "hello\n"),
        ("src/support.ts", "export const support = 1;\n"),
        ("ignored/skip.fake", "hello\n"),
        (".aftignore", "ignored/\n"),
    ]);
    let fake_bin_dir = project.path().join("bin");
    fs::create_dir_all(&fake_bin_dir).expect("create fake server bin dir");
    let fake_binary_name = fake_server
        .file_name()
        .expect("fake server file name")
        .to_string_lossy()
        .to_string();
    let installed_fake_server = fake_bin_dir.join(&fake_binary_name);
    link_cached_path(&fake_server, &installed_fake_server).expect("link cached fake server");
    // The fake LSP has no CLI probe; closed stdin makes its protocol loop exit.
    warm_executable(&installed_fake_server, &[]);

    let mut aft = AftProcess::spawn_with_env(&[("AFT_FAKE_LSP_PULL", std::ffi::OsStr::new("1"))]);
    let configure = send(
        &mut aft,
        json!({
            "id": "cfg-inspect-aftignore",
            "command": "configure",
            "harness": "opencode",
            "project_root": project.path(),
            "lsp_paths_extra": [fake_bin_dir],
            "config": user_config(serde_json::json!({
                "lsp": {
                    "disabled": ["typescript", "oxlint", "biome"],
                    "servers": {
                        "fake": {
                            "extensions": ["fake"],
                            "binary": fake_binary_name,
                            "args": [],
                            "root_markers": ["fake.toml"]
                        }
                    }
                }
            }))
        }),
    );
    assert_eq!(
        configure["success"], true,
        "configure should succeed: {configure:?}"
    );

    let diagnostics = send(
        &mut aft,
        json!({
            "id": "warm-inspect-aftignore-diagnostics",
            "command": "lsp_diagnostics",
            "file": project.path().join("src/main.fake"),
            "wait_ms": 2_000,
        }),
    );
    assert_eq!(
        diagnostics["success"], true,
        "fake LSP diagnostics warm-up should succeed: {diagnostics:?}"
    );
    assert_eq!(
        diagnostics["complete"], true,
        "fake LSP diagnostics warm-up should complete: {diagnostics:?}"
    );
    assert_eq!(
        diagnostics["total"], 1,
        "fake LSP diagnostics warm-up should report the main fixture: {diagnostics:?}"
    );
    let support_lsp = send(
        &mut aft,
        json!({
            "id": "inspect-support-lsp",
            "command": "lsp_inspect",
            "file": project.path().join("src/support.ts"),
        }),
    );
    assert_eq!(
        support_lsp["matching_servers"],
        json!([]),
        "the Tier-2 support file must not add an unchecked diagnostics producer: {support_lsp:?}"
    );

    let request = json!({
        "id": "inspect-diagnostics-aftignore",
        "command": "inspect",
        "sections": ["diagnostics"],
        "scope": ".",
        "topK": 10,
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    let response = loop {
        let response = send(&mut aft, request.clone());
        if response["success"] == true {
            break response;
        }
        assert_eq!(
            response["failure_reason"], "inspect_not_fresh",
            "inspect failed for an unexpected reason: {response:?}"
        );
        assert!(
            Instant::now() < deadline,
            "inspect should become fresh after fake LSP collection: {response:?}"
        );
        thread::sleep(Duration::from_millis(50));
    };

    assert_eq!(
        response["success"], true,
        "inspect should succeed: {response:?}"
    );
    assert_eq!(
        response["inspect_terminal"], "fresh",
        "successful scoped diagnostics inspect must be fresh: {response:?}"
    );
    assert_eq!(response["summary"]["diagnostics"]["errors"], 1);
    let details = response["details"]["diagnostics"]
        .as_array()
        .expect("diagnostics details");
    assert_eq!(
        details.len(),
        1,
        ".aftignored file must be skipped by scoped inspect diagnostics: {response:?}"
    );
    assert_eq!(details[0]["file"], "src/main.fake");
    assert_eq!(details[0]["message"], "test pull diagnostic");

    let status = aft.shutdown();
    assert!(status.success());
}
/// Helper: run a glob tool_call and return the set of matched files (normalized).
fn glob_files(aft: &mut AftProcess, id: &str, pattern: &str, path: Option<&str>) -> Vec<String> {
    let mut req = json!({
        "id": id,
        "command": "glob",
        "pattern": pattern,
    });
    if let Some(p) = path {
        req["path"] = json!(p);
    }
    let response = send(aft, req);
    assert_eq!(
        response["success"], true,
        "glob should succeed: {response:?}"
    );
    response["files"]
        .as_array()
        .expect("files array")
        .iter()
        .map(|entry| entry.as_str().expect("file path").replace('\\', "/"))
        .collect()
}

/// A bare filename pattern with a `path` directory must match a file directly
/// under that directory. The pattern is relative to `path`, so `{path: "src",
/// pattern: "a.rs"}` must return `src/a.rs`. This is the standard glob contract:
/// `path` is the search root and the pattern is evaluated relative to it.
#[test]
fn glob_bare_filename_matches_top_level_under_path_fallback() {
    let project = setup_project(&[("src/a.rs", "fn a() {}\n"), ("src/sub/b.rs", "fn b() {}\n")]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    // Bare filename at the top level of the search root — must match.
    let files = glob_files(&mut aft, "glob-bare-top", "a.rs", Some("src"));
    let expected = canonical_path_string(&project.path().join("src/a.rs")).replace('\\', "/");
    assert_eq!(
        files,
        vec![expected.clone()],
        "bare filename must match a file directly under path (fallback): {files:?}"
    );

    // Bare filename must NOT match a file in a subdirectory — `b.rs` is at
    // `src/sub/b.rs`, so relative to `src` it is `sub/b.rs`, which `b.rs`
    // does not match.
    let files = glob_files(&mut aft, "glob-bare-nested", "b.rs", Some("src"));
    assert!(
        files.is_empty(),
        "bare filename must not match nested file under path (fallback): {files:?}"
    );

    // `**/b.rs` must match the nested file.
    let files = glob_files(&mut aft, "glob-doublestar-b", "**/b.rs", Some("src"));
    let expected_b = canonical_path_string(&project.path().join("src/sub/b.rs")).replace('\\', "/");
    assert_eq!(
        files,
        vec![expected_b],
        "**/b.rs must match nested file under path (fallback): {files:?}"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

/// Same as above but with the search index enabled and ready, so the indexed
/// route is exercised. The indexed and fallback routes must agree.
#[test]
fn glob_bare_filename_matches_top_level_under_path_indexed() {
    let project = setup_project(&[
        (
            ".fixture-id",
            "glob_bare_filename_matches_top_level_under_path_indexed\n",
        ),
        ("src/a.rs", "fn a() {}\n"),
        ("src/sub/b.rs", "fn b() {}\n"),
    ]);
    let mut aft = AftProcess::spawn();
    configure_with_index(&mut aft, project.path());

    // Wait for index to be ready.
    wait_for_index_ready(&mut aft, || {
        json!({
            "id": "glob-index-ready-probe",
            "command": "grep",
            "pattern": "fn a",
            "include": ["src/**/*.rs"],
        })
    });

    // Bare filename at the top level of the search root — must match.
    let files = glob_files(&mut aft, "glob-bare-top-idx", "a.rs", Some("src"));
    let expected = canonical_path_string(&project.path().join("src/a.rs")).replace('\\', "/");
    assert_eq!(
        files,
        vec![expected.clone()],
        "bare filename must match a file directly under path (indexed): {files:?}"
    );

    // Bare filename must NOT match a file in a subdirectory.
    let files = glob_files(&mut aft, "glob-bare-nested-idx", "b.rs", Some("src"));
    assert!(
        files.is_empty(),
        "bare filename must not match nested file under path (indexed): {files:?}"
    );

    // `**/b.rs` must match the nested file.
    let files = glob_files(&mut aft, "glob-doublestar-b-idx", "**/b.rs", Some("src"));
    let expected_b = canonical_path_string(&project.path().join("src/sub/b.rs")).replace('\\', "/");
    assert_eq!(
        files,
        vec![expected_b],
        "**/b.rs must match nested file under path (indexed): {files:?}"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

// ---------------------------------------------------------------------------
// Explicitly named ignored / symlinked directories (ripgrep convention): a
// directory the caller names in `path` is searched even when an ignore rule
// matches it, and a symlinked root is resolved and walked. Without `path`,
// the ignored directory stays out of project-wide results.
// ---------------------------------------------------------------------------

#[cfg(unix)]
const DEP_NEEDLE: &str = "createBindingNeedle";

/// Project with a gitignored `node_modules/pkg/` holding a matching file, a
/// package directory `node_modules/linked` that is a symlink into a store
/// folder (the bun layout), and one tracked source file that also matches.
#[cfg(unix)]
fn setup_dependency_project(fixture_id: &str) -> tempfile::TempDir {
    let project = setup_project(&[
        (".fixture-id", fixture_id),
        (".gitignore", "node_modules/\n"),
        ("src/app.js", "export const createBindingNeedle = 0;\n"),
        (
            "node_modules/pkg/index.js",
            "export function createBindingNeedle() { return 1; }\n",
        ),
        (
            "node_modules/.store/linked@1.0.0/node_modules/linked/index.cjs",
            "exports.createBindingNeedle = function () { return 2; };\n",
        ),
    ]);
    std::os::unix::fs::symlink(
        ".store/linked@1.0.0/node_modules/linked",
        project.path().join("node_modules/linked"),
    )
    .expect("create package symlink");
    project
}

#[cfg(unix)]
fn grep_in(aft: &mut AftProcess, id: &str, path: Option<&str>) -> Value {
    let mut request = json!({
        "id": id,
        "command": "grep",
        "pattern": DEP_NEEDLE,
    });
    if let Some(path) = path {
        request["path"] = json!(path);
    }
    let response = send(aft, request);
    assert_eq!(
        response["success"], true,
        "grep should succeed: {response:?}"
    );
    response
}

#[cfg(unix)]
fn matched_file_names(response: &Value) -> Vec<String> {
    let mut names: Vec<String> = response["matches"]
        .as_array()
        .expect("matches array")
        .iter()
        .map(|entry| {
            entry["file"]
                .as_str()
                .expect("match file")
                .replace('\\', "/")
        })
        .collect();
    names.sort();
    names
}

#[cfg(unix)]
fn assert_explicit_dependency_dirs_searched(aft: &mut AftProcess, mode: &str) {
    let ignored = grep_in(
        aft,
        &format!("grep-ignored-dir-{mode}"),
        Some("node_modules/pkg"),
    );
    let files = matched_file_names(&ignored);
    assert_eq!(
        ignored["total_matches"], 1,
        "{mode}: an explicitly named gitignored directory must be searched: {ignored:?}"
    );
    assert!(
        files[0].ends_with("node_modules/pkg/index.js"),
        "{mode}: unexpected match file {files:?}"
    );
    assert_eq!(
        ignored["no_files_matched_scope"], false,
        "{mode}: {ignored:?}"
    );

    let linked = grep_in(
        aft,
        &format!("grep-linked-dir-{mode}"),
        Some("node_modules/linked"),
    );
    let files = matched_file_names(&linked);
    assert_eq!(
        linked["total_matches"], 1,
        "{mode}: an explicitly named symlinked package directory must be searched: {linked:?}"
    );
    assert!(files[0].ends_with("index.cjs"), "{mode}: {files:?}");
    assert_eq!(
        linked["no_files_matched_scope"], false,
        "{mode}: {linked:?}"
    );

    let ignored_glob = glob_files(
        aft,
        &format!("glob-ignored-dir-{mode}"),
        "*",
        Some("node_modules/pkg"),
    );
    assert_eq!(ignored_glob.len(), 1, "{mode}: {ignored_glob:?}");
    assert!(
        ignored_glob[0].ends_with("node_modules/pkg/index.js"),
        "{mode}: {ignored_glob:?}"
    );

    let linked_glob = glob_files(
        aft,
        &format!("glob-linked-dir-{mode}"),
        "*",
        Some("node_modules/linked"),
    );
    assert_eq!(linked_glob.len(), 1, "{mode}: {linked_glob:?}");
    assert!(
        linked_glob[0].ends_with("index.cjs"),
        "{mode}: {linked_glob:?}"
    );

    // Project-wide search keeps the ignored directory out.
    let project_wide = grep_in(aft, &format!("grep-project-wide-{mode}"), None);
    let files = matched_file_names(&project_wide);
    assert_eq!(
        files.len(),
        1,
        "{mode}: project-wide grep must not search the ignored directory: {project_wide:?}"
    );
    assert!(files[0].ends_with("src/app.js"), "{mode}: {files:?}");
    let project_glob = glob_files(
        aft,
        &format!("glob-project-wide-{mode}"),
        "**/index.*",
        None,
    );
    assert!(
        project_glob.is_empty(),
        "{mode}: project-wide glob must not list the ignored directory: {project_glob:?}"
    );
}

#[cfg(unix)]
#[test]
fn explicit_ignored_and_symlinked_dirs_are_searched_fallback() {
    let project = setup_dependency_project("explicit_ignored_dirs_fallback\n");
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    assert_explicit_dependency_dirs_searched(&mut aft, "fallback");

    let status = aft.shutdown();
    assert!(status.success());
}

#[cfg(unix)]
#[test]
fn explicit_ignored_and_symlinked_dirs_are_searched_indexed() {
    let project = setup_dependency_project("explicit_ignored_dirs_indexed\n");
    let mut aft = AftProcess::spawn();
    configure_with_index(&mut aft, project.path());
    wait_for_index_ready(&mut aft, || {
        json!({
            "id": "explicit-ignored-index-probe",
            "command": "grep",
            "pattern": DEP_NEEDLE,
        })
    });

    assert_explicit_dependency_dirs_searched(&mut aft, "indexed");

    let status = aft.shutdown();
    assert!(status.success());
}

#[cfg(unix)]
#[test]
fn explicit_symlink_escaping_root_is_refused_when_restricted() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("project");
    let outside = dir.path().join("outside-store/pkg");
    fs::create_dir_all(root.join("node_modules")).expect("create project");
    fs::create_dir_all(&outside).expect("create outside store");
    fs::write(root.join(".gitignore"), "node_modules/\n").expect("write gitignore");
    fs::write(
        outside.join("index.js"),
        "export const createBindingNeedle = 3;\n",
    )
    .expect("write outside file");
    std::os::unix::fs::symlink(&outside, root.join("node_modules/escape"))
        .expect("create escaping symlink");

    let mut aft = AftProcess::spawn();
    let resp = send(
        &mut aft,
        json!({
            "id": "cfg-restricted",
            "command": "configure",
            "harness": "opencode",
            "project_root": root,
            "config": user_config(json!({
                "restrict_to_project_root": true,
                "indexes": { "trigram": false }
            })),
        }),
    );
    assert_eq!(resp["success"], true, "configure should succeed: {resp:?}");

    for command in ["grep", "glob"] {
        let response = send(
            &mut aft,
            json!({
                "id": format!("{command}-escaping-symlink"),
                "command": command,
                "pattern": if command == "grep" { DEP_NEEDLE } else { "*" },
                "path": "node_modules/escape",
            }),
        );
        assert_eq!(
            response["success"], false,
            "{command}: a symlink resolving outside the root must be refused: {response:?}"
        );
        assert_eq!(
            response["code"], "path_outside_root",
            "{command}: {response:?}"
        );
    }

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn filtered_scope_distinguishes_no_files_from_no_matches() {
    for indexed in [false, true] {
        let project = setup_project(&[("src/a.txt", "hello\n"), ("src/b.rs", "hello\n")]);
        fs::create_dir(project.path().join("empty")).unwrap();
        let mut aft = AftProcess::spawn();
        if indexed {
            configure_with_index(&mut aft, project.path());
            wait_for_index_ready(
                &mut aft,
                || json!({"id":"ready", "command":"grep", "pattern":"hello"}),
            );
        } else {
            configure(&mut aft, project.path());
        }
        for (extra, empty) in [
            (json!({}), false),
            (json!({"include":"*.nosuchext"}), true),
            (json!({"path": project.path().join("empty")}), true),
            (json!({"include":"*.rs"}), false),
            (json!({"exclude":["*.rs", "*.txt"]}), true),
        ] {
            let mut request = json!({"id":"filtered-scope", "command":"grep", "pattern":"ZQNOPE"});
            request
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            let response = send(&mut aft, request);
            assert_eq!(response["success"], true, "{response}");
            assert_eq!(
                response["no_files_matched_scope"], empty,
                "indexed={indexed}: {response}"
            );
            assert_eq!(
                response["text"]
                    .as_str()
                    .unwrap()
                    .contains("nothing was searched"),
                empty
            );
        }
        for (pattern, empty) in [("*.nosuchext", true), ("**/*.rs", false)] {
            let response = send(
                &mut aft,
                json!({"id":"glob-filtered", "command":"glob", "pattern":pattern}),
            );
            assert_eq!(
                response["no_files_matched_scope"], empty,
                "indexed={indexed}: {response}"
            );
            assert_eq!(
                response["text"]
                    .as_str()
                    .unwrap()
                    .contains("nothing was searched"),
                empty
            );
        }
        assert!(aft.shutdown().success());
    }
}

mod explicit_target_regressions {
    use super::*;

    fn named_ignored_folder_project() -> tempfile::TempDir {
        setup_project(&[
            (".gitignore", ".cortexkit/internal-docs/\n*ancestor-cost*\n"),
            ("src/anchor.md", "index anchor\n"),
            (
                ".cortexkit/internal-docs/designs/ancestor-cost.md",
                "cost design\n",
            ),
            (".cortexkit/internal-docs/.gitignore", "hidden-cost.md\n"),
            (
                ".cortexkit/internal-docs/designs/.aftignore",
                "private-cost.md\n",
            ),
            (".cortexkit/internal-docs/hidden-cost.md", "hidden\n"),
            (
                ".cortexkit/internal-docs/designs/private-cost.md",
                "private\n",
            ),
            (
                ".cortexkit/internal-docs/node_modules/pkg/cost.md",
                "dependency\n",
            ),
            (".cortexkit/internal-docs/target/debug/cost.md", "build\n"),
            (".cortexkit/internal-docs/.git/cost.md", "git\n"),
            (".cortexkit/internal-docs/cost.sqlite", "database\n"),
            (".cortexkit/internal-docs/cost.sqlite-wal", "wal\n"),
            (".cortexkit/internal-docs/cost.sqlite-shm", "shm\n"),
            (".cortexkit/internal-docs/cost.sqlite-journal", "journal\n"),
        ])
    }

    #[test]
    fn glob_named_ignored_folder_overrides_only_ancestor_ignore_rules() {
        for indexed in [false, true] {
            let project = named_ignored_folder_project();
            let mut aft = AftProcess::spawn();
            if indexed {
                configure_with_index(&mut aft, project.path());
                wait_for_index_ready(
                    &mut aft,
                    || json!({"id":"ready", "command":"grep", "pattern":"anchor"}),
                );
            } else {
                configure(&mut aft, project.path());
            }
            for (path, pattern) in [
                (None, ".cortexkit/internal-docs/**/*cost*"),
                (Some(".cortexkit"), "internal-docs/**/*cost*"),
                (None, ".cortexkit/internal-docs/designs/*cost*"),
                (None, ".cortexkit/internal-docs/designs/ancestor-cost.md"),
            ] {
                let mut request =
                    json!({"id":"named-ignored", "command":"glob", "pattern":pattern});
                if let Some(path) = path {
                    request["path"] = json!(path);
                }
                let response = send(&mut aft, request);
                assert_eq!(response["success"], true, "indexed={indexed}: {response}");
                assert_eq!(response["total"], 1, "indexed={indexed}: {response}");
                assert_eq!(response["fallback"], "filesystem", "{response}");
                assert!(
                    response["files"][0]
                        .as_str()
                        .unwrap()
                        .replace('\\', "/")
                        .ends_with("designs/ancestor-cost.md"),
                    "{response}"
                );
            }
            let response = send(
                &mut aft,
                json!({
                    "id":"unnamed-ignored", "command":"glob", "pattern":"**/*.md"
                }),
            );
            assert_eq!(response["total"], 1, "indexed={indexed}: {response}");
            assert!(
                response["files"][0]
                    .as_str()
                    .unwrap()
                    .ends_with("anchor.md"),
                "{response}"
            );
            let response = send(
                &mut aft,
                json!({
                    "id":"visible-prefix", "command":"glob", "pattern":"src/*.md"
                }),
            );
            assert_eq!(response["total"], 1, "{response}");
            if indexed {
                assert_eq!(response["fallback"], Value::Null, "{response}");
            }
            assert!(aft.shutdown().success());
        }
    }

    #[test]
    fn glob_named_ignored_folder_never_overrides_builtin_directories() {
        let project = named_ignored_folder_project();
        let mut aft = AftProcess::spawn();
        configure(&mut aft, project.path());
        for directory in ["node_modules/pkg", "target/debug", ".git"] {
            let response = send(
                &mut aft,
                json!({
                    "id":"builtin", "command":"glob",
                    "pattern":format!(".cortexkit/internal-docs/{directory}/*cost*")
                }),
            );
            assert_eq!(response["total"], 0, "{response}");
        }
        assert!(aft.shutdown().success());
    }

    #[test]
    fn glob_named_ignored_folder_cannot_escape_restricted_project() {
        let project = named_ignored_folder_project();
        let outside = setup_project(&[
            (".gitignore", "ignored/\n"),
            ("ignored/cost.md", "outside\n"),
        ]);
        let mut aft = AftProcess::spawn();
        let response = send(
            &mut aft,
            json!({
                "id":"restricted", "command":"configure", "harness":"opencode", "project_root":project.path(),
                "config":user_config(json!({"restrict_to_project_root":true, "indexes":{"trigram":false}}))
            }),
        );
        assert_eq!(response["success"], true, "{response}");
        let mut patterns = vec![format!("{}/ignored/*cost*", outside.path().display())];
        patterns.push(format!(
            "../{}/ignored/*cost*",
            outside.path().file_name().unwrap().to_string_lossy()
        ));
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(
                outside.path().join("ignored"),
                project.path().join("escape"),
            )
            .unwrap();
            patterns.push("escape/*cost*".to_string());
        }
        for pattern in patterns {
            let response = send(
                &mut aft,
                json!({"id":"escape", "command":"glob", "pattern":pattern}),
            );
            assert!(
                response["success"] == false || response["total"] == 0,
                "{response}"
            );
        }
        assert!(aft.shutdown().success());
    }

    fn ignored_reports_project() -> tempfile::TempDir {
        setup_project(&[
            (".git/info/exclude", ".cortexkit/alfonso/reports/\n"),
            (".gitignore", "ignored-by-gitignore/\n"),
            ("src/anchor.txt", "index anchor\n"),
            (
                ".cortexkit/alfonso/reports/common-auth-body-positive.log",
                "17 pass; 0 fail\n",
            ),
            (
                ".cortexkit/alfonso/reports/.gitignore",
                "common-auth-body-hidden.log\nnested/\n",
            ),
            (
                ".cortexkit/alfonso/reports/common-auth-body-hidden.log",
                "18 pass; 0 fail\n",
            ),
            (
                ".cortexkit/alfonso/reports/nested/common-auth-body-nested.log",
                "19 pass; 0 fail\n",
            ),
            ("ignored-by-gitignore/report.log", "20 pass; 0 fail\n"),
        ])
    }

    fn reports_request(command: &str, path: Option<&str>) -> Value {
        let mut request = json!({"id": "reports", "command": command,
        "pattern": if command == "grep" { r"\d+ pass; \d+ fail" } else { "common-auth-body-*.log" }});
        if command == "grep" {
            request["include"] = json!("common-auth-body-*.log");
        }
        if let Some(path) = path {
            request["path"] = json!(path);
        }
        request
    }

    #[test]
    fn explicit_ignored_reports_directory_with_include_is_searched() {
        for indexed in [false, true] {
            let project = ignored_reports_project();
            let mut aft = AftProcess::spawn();
            if indexed {
                configure_with_index(&mut aft, project.path());
                wait_for_index_ready(
                    &mut aft,
                    || json!({"id":"ready", "command":"grep", "pattern":"anchor"}),
                );
            } else {
                configure(&mut aft, project.path());
            }
            for path in [".cortexkit/alfonso/reports", "ignored-by-gitignore"] {
                let mut request = reports_request("grep", Some(path));
                if path == "ignored-by-gitignore" {
                    request["include"] = json!("*.log");
                }
                let response = send(&mut aft, request);
                assert_eq!(
                    response["total_matches"], 1,
                    "indexed={indexed}: {response}"
                );
                assert_eq!(response["files_searched"], 1, "{response}");
                assert_eq!(response["fallback"], "filesystem", "{response}");
                let expected_name = if path == "ignored-by-gitignore" {
                    "report.log"
                } else {
                    "common-auth-body-positive.log"
                };
                assert!(
                    response["matches"][0]["file"]
                        .as_str()
                        .unwrap()
                        .ends_with(expected_name),
                    "{response}"
                );
            }
            let response = send(
                &mut aft,
                reports_request("glob", Some(".cortexkit/alfonso/reports")),
            );
            assert_eq!(response["total"], 1, "indexed={indexed}: {response}");
            assert_eq!(response["fallback"], "filesystem", "{response}");
            assert!(
                response["files"][0]
                    .as_str()
                    .unwrap()
                    .ends_with("common-auth-body-positive.log"),
                "{response}"
            );
            assert!(aft.shutdown().success());
        }
    }

    #[test]
    fn explicit_ignored_reports_directory_with_exact_name_include_list_is_searched() {
        for indexed in [false, true] {
            let project = setup_project(&[
                (".git/info/exclude", ".cortexkit/alfonso/reports/\n"),
                ("src/anchor.txt", "index anchor\n"),
                (
                    ".cortexkit/alfonso/reports/a.stderr.log",
                    "command failed\nsecond error\n",
                ),
                (
                    ".cortexkit/alfonso/reports/b.stderr.log",
                    "command failed\n",
                ),
                (
                    ".cortexkit/alfonso/reports/c.stderr.log",
                    "error one\nerror two\nerror three\n",
                ),
                (
                    ".cortexkit/alfonso/reports/not-listed.stderr.log",
                    "command failed\nerror outside the include list\n",
                ),
            ]);
            let mut aft = AftProcess::spawn();
            if indexed {
                configure_with_index(&mut aft, project.path());
                wait_for_index_ready(
                    &mut aft,
                    || json!({"id":"ready", "command":"grep", "pattern":"anchor"}),
                );
            } else {
                configure(&mut aft, project.path());
            }

            let response = send(
                &mut aft,
                json!({
                    "id": format!("exact-include-indexed-{indexed}"),
                    "command": "grep",
                    "pattern": "(fail)|error",
                    "path": ".cortexkit/alfonso/reports",
                    // The agent-facing comma list arrives in this translated form.
                    "include": ["**/a.stderr.log", "**/b.stderr.log", "**/c.stderr.log"],
                }),
            );
            assert_eq!(response["success"], true, "indexed={indexed}: {response}");
            assert_eq!(
                response["total_matches"], 6,
                "indexed={indexed}: {response}"
            );
            assert_eq!(
                response["files_searched"], 3,
                "indexed={indexed}: {response}"
            );
            assert_eq!(
                response["files_with_matches"], 3,
                "indexed={indexed}: {response}"
            );
            assert_eq!(
                response["fallback"], "filesystem",
                "indexed={indexed}: {response}"
            );

            let mut matched_names: Vec<String> = response["matches"]
                .as_array()
                .expect("matches array")
                .iter()
                .map(|matched| {
                    matched["file"]
                        .as_str()
                        .expect("matched file")
                        .replace('\\', "/")
                        .rsplit('/')
                        .next()
                        .unwrap_or_default()
                        .to_string()
                })
                .collect();
            matched_names.sort();
            assert_eq!(
                matched_names,
                [
                    "a.stderr.log",
                    "a.stderr.log",
                    "b.stderr.log",
                    "c.stderr.log",
                    "c.stderr.log",
                    "c.stderr.log",
                ],
                "indexed={indexed}: {response}"
            );
            assert!(aft.shutdown().success());
        }
    }

    #[test]
    fn explicit_ignored_reports_single_file_is_searched() {
        let project = ignored_reports_project();
        let mut aft = AftProcess::spawn();
        configure(&mut aft, project.path());
        let response = send(
            &mut aft,
            reports_request(
                "grep",
                Some(".cortexkit/alfonso/reports/common-auth-body-hidden.log"),
            ),
        );
        assert_eq!(response["total_matches"], 1, "{response}");
        assert_eq!(response["files_searched"], 1, "{response}");
        let response = send(
            &mut aft,
            reports_request(
                "glob",
                Some(".cortexkit/alfonso/reports/common-auth-body-hidden.log"),
            ),
        );
        assert_eq!(response["total"], 1, "{response}");
        assert!(aft.shutdown().success());
    }

    #[test]
    fn non_explicit_reports_search_skips_ignored_files() {
        let project = ignored_reports_project();
        let mut aft = AftProcess::spawn();
        configure(&mut aft, project.path());
        let mut request = reports_request("grep", None);
        // Use a project-relative glob that would admit these logs if the
        // ignore filter were missing, rather than a root-only filename glob.
        request["include"] = json!("**/*.log");
        let response = send(&mut aft, request);
        assert_eq!(response["total_matches"], 0, "{response}");
        let mut request = reports_request("glob", None);
        request["pattern"] = json!("**/common-auth-body-*.log");
        let response = send(&mut aft, request);
        assert_eq!(response["total"], 0, "{response}");
        assert!(aft.shutdown().success());
    }

    #[test]
    fn empty_reports_scope_names_ignore_and_include_exclusions() {
        for indexed in [false, true] {
            let project = ignored_reports_project();
            let mut aft = AftProcess::spawn();
            if indexed {
                configure_with_index(&mut aft, project.path());
                wait_for_index_ready(
                    &mut aft,
                    || json!({"id":"ready", "command":"grep", "pattern":"anchor"}),
                );
            } else {
                configure(&mut aft, project.path());
            }
            for command in ["grep", "glob"] {
                let response = send(
                    &mut aft,
                    reports_request(command, Some(".cortexkit/alfonso")),
                );
                let text = response["text"].as_str().unwrap();
                assert!(
                    text.contains("1 ignored items (directories counted once)"),
                    "indexed={indexed}: {response}"
                );
                let hint = if command == "glob" {
                    "name the ignored folder in the pattern to search it"
                } else {
                    "pass the file path directly to search them"
                };
                assert!(text.contains(hint), "{response}");
                assert!(!text.contains("No searchable files exist"), "{response}");
                let mut request = reports_request(command, Some("src"));
                if command == "grep" {
                    request["include"] = json!("*.missing");
                } else {
                    request["pattern"] = json!("*.missing");
                }
                let response = send(&mut aft, request);
                let text = response["text"].as_str().unwrap();
                assert!(
                    text.contains("include pattern matched none of 1 files"),
                    "indexed={indexed}: {response}"
                );
                assert!(!text.contains("No searchable files exist"), "{response}");
            }
            assert!(aft.shutdown().success());
        }
    }
}
