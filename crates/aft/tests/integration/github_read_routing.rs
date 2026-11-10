#![cfg(unix)]

use std::ffi::OsStr;
use std::fs;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use aft::commands::read::build_read_outcome;
use aft::config::Config;
use aft::context::{default_language_provider_factory, AppContext};
use aft::protocol::RawRequest;
use aft::response_finalize::DispatchOutcome;
use aft::subc_translate::{subc_translate_with_context, TranslateContext};
use serde_json::{json, Value};

use super::helpers::{link_cached_executable, AftProcess};

fn write_fake_gh(bin_dir: &Path) {
    fs::create_dir_all(bin_dir).expect("create fake gh directory");
    let script = bin_dir.join("gh");
    link_cached_executable(
        &script,
        br#"#!/bin/sh
if [ -n "${AFT_TEST_GH_STARTED:-}" ]; then
  : > "$AFT_TEST_GH_STARTED"
fi
if [ -n "${AFT_TEST_GH_DELAY_SECONDS:-}" ] && [ "$1" != "api" ]; then
  sleep "$AFT_TEST_GH_DELAY_SECONDS"
fi
case "$1" in
  issue)
    printf '%s\n' '{"number":7,"title":"Fixture issue","state":"OPEN","body":"one\ntwo\nthree\nhttps://github.com/user-attachments/files/7/screenshot.png","url":"https://github.com/owner/repo/issues/7","comments":[]}'
    ;;
  pr)
    printf '%s\n' '{"number":9,"title":"Fixture pull request","state":"OPEN","body":"pull request body","url":"https://github.com/owner/repo/pull/9","comments":[],"files":[],"reviews":[]}'
    ;;
  api)
    printf '%s\n' '{"data":{"repository":{"nameWithOwner":"owner/repo","pullRequest":{"number":9,"title":"Fixture pull request","state":"OPEN","body":"pull request body","comments":[],"files":[],"reviews":[]}}}}'
    ;;
  *)
    exit 1
    ;;
esac
"#,
    )
    .expect("cache fake gh");
}

fn spawned_with_fake_gh(bin_dir: &Path) -> AftProcess {
    let original_path = std::env::var_os("PATH").unwrap_or_default();
    let path = std::env::join_paths(
        std::iter::once(bin_dir.to_path_buf()).chain(std::env::split_paths(&original_path)),
    )
    .expect("join fake gh path entries");
    AftProcess::spawn_with_env(&[("PATH", path.as_os_str())])
}

fn spawned_with_slow_fake_gh(bin_dir: &Path, started: &Path) -> AftProcess {
    let original_path = std::env::var_os("PATH").unwrap_or_default();
    let path = std::env::join_paths(
        std::iter::once(bin_dir.to_path_buf()).chain(std::env::split_paths(&original_path)),
    )
    .expect("join fake gh path entries");
    AftProcess::spawn_with_env(&[
        ("PATH", path.as_os_str()),
        ("AFT_TEST_GH_STARTED", started.as_os_str()),
        ("AFT_TEST_GH_DELAY_SECONDS", OsStr::new("2")),
    ])
}

/// Configure a throwaway project with the gh_read gate enabled at the USER
/// tier (the only tier that can enable it; project tiers drop the key). The
/// gate's default-off contract is pinned by the dedicated disabled-read tests.
fn configure_gh_read_enabled(aft: &mut AftProcess, project: &Path) {
    let user_config = project.join("user-aft.jsonc");
    fs::write(&user_config, "{\"github\": {\"read\": true}}").expect("write gh_read user config");
    let response = aft.send(
        &json!({
            "id": "configure-gh-read",
            "command": "configure",
            "harness": "opencode",
            "project_root": project,
            "cortexkit_user_config_path": user_config,
        })
        .to_string(),
    );
    assert_eq!(response["success"], true, "configure failed: {response:#}");
}

fn read_response(aft: &mut AftProcess, id: &str, file: &str, params: Value) -> Value {
    let mut request = params.as_object().cloned().expect("read params object");
    request.insert("id".to_string(), Value::String(id.to_string()));
    request.insert("command".to_string(), Value::String("read".to_string()));
    request.insert("file".to_string(), Value::String(file.to_string()));
    aft.send(&Value::Object(request).to_string())
}

fn write_timeline_fake_gh(bin_dir: &Path) {
    fs::create_dir_all(bin_dir).expect("create timeline fixture gh directory");
    let script = bin_dir.join("gh");
    link_cached_executable(
        &script,
        br#"#!/bin/sh
case "$1:$2" in
  pr:view)
    cat <<'JSON'
{"number":999,"title":"Timeline fixture","state":"OPEN","author":{"login":"author"},"createdAt":"2026-09-03T05:00:00Z","updatedAt":"2026-09-03T07:00:00Z","labels":{"nodes":[{"name":"bug"}]},"body":"Fixture body","url":"https://github.com/cortexkit/aft/pull/999","baseRefName":"main","headRefName":"timeline-fixture","reviewDecision":"APPROVED","comments":{"nodes":[{"author":{"login":"commenter"},"body":"First discussion comment","createdAt":"2026-09-03T05:10:00Z"},{"author":{"login":"commenter"},"body":"Last discussion comment","createdAt":"2026-09-03T06:50:00Z"}]},"files":{"nodes":[{"path":"src/timeline.rs","additions":3,"deletions":1}]},"reviews":{"nodes":[{"author":{"login":"reviewer-one"},"body":"Looks good","state":"APPROVED","submittedAt":"2026-09-03T05:20:00Z"},{"author":{"login":"reviewer-two"},"body":"Please address this","state":"CHANGES_REQUESTED","submittedAt":"2026-09-03T05:30:00Z"}]}}
JSON
    ;;
  api:graphql)
    cat <<'JSON'
{"data":{"repository":{"nameWithOwner":"cortexkit/aft","pullRequest":{"number":999,"reviews":{"nodes":[{"author":{"login":"reviewer-one"},"body":"Looks good","state":"APPROVED","submittedAt":"2026-09-03T05:20:00Z","comments":{"totalCount":2,"nodes":[{"author":{"login":"inline-one"},"body":"Inline comment one","createdAt":"2026-09-03T05:40:00Z","path":"src/timeline.rs","line":12},{"author":{"login":"inline-two"},"body":"Inline comment two","createdAt":"2026-09-03T05:45:00Z","path":"src/timeline.rs","line":24}]}}]}}}}}
JSON
    ;;
  api:repos/*)
    cat <<'JSON'
[[{"event":"labeled","created_at":"2026-09-03T05:05:00Z","actor":{"login":"maintainer"},"label":{"name":"bug"}},{"event":"closed","created_at":"2026-09-03T06:57:00Z","actor":{"login":"aft-alfonso[bot]"},"commit_id":"0123456789abcdef"},{"event":"reopened","created_at":"2026-09-03T07:00:00Z","actor":{"login":"maintainer"}}]]
JSON
    ;;
  *)
    printf 'unexpected timeline fixture gh command: %s %s\n' "$1" "$2" >&2
    exit 1
    ;;
esac
"#,
    )
    .expect("cache timeline fixture gh script");
}

fn ordinals_with_prefix(text: &str, prefix: &str) -> std::collections::BTreeSet<usize> {
    text.lines()
        .filter_map(|line| {
            let start = line.find(prefix)? + prefix.len();
            let end = line[start..].find(']')? + start;
            line[start..end].parse().ok()
        })
        .collect()
}

#[test]
fn github_read_routes_short_and_explicit_forms_without_filesystem_rendering() {
    let temp = tempfile::tempdir().expect("create fake gh root");
    write_fake_gh(&temp.path().join("bin"));
    let mut aft = spawned_with_fake_gh(&temp.path().join("bin"));
    configure_gh_read_enabled(&mut aft, temp.path());

    let issue = read_response(&mut aft, "github-issue", "issue://7", json!({}));
    assert_eq!(issue["success"], true, "issue read failed: {issue:#}");
    assert_eq!(issue["content"].as_str(), Some("# Issue #7: Fixture issue\n\nRepository: owner/repo\nState: OPEN\n\n## Body\n\none\ntwo\nthree\nhttps://github.com/user-attachments/files/7/screenshot.png\n\nDiscussion drill-down: issue://7/comments/<sel> (for example 3, 3-5, 3,7, or -1).\n\n"));
    assert_eq!(
        issue["attachments"],
        json!([]),
        "missing vision capability must be false"
    );
    assert!(
        !issue["content"]
            .as_str()
            .unwrap_or_default()
            .starts_with("1: "),
        "GitHub text must remain the engine's canonical bytes"
    );

    let pull_request = read_response(&mut aft, "github-pr", "pr://owner/repo/9", json!({}));
    assert_eq!(
        pull_request["success"], true,
        "explicit PR read failed: {pull_request:#}"
    );
    assert_eq!(
        pull_request["content"].as_str(),
        Some("# Pull request #9: Fixture pull request\n\nRepository: owner/repo\nState: OPEN\nReview thread state couldn't be read; unresolved threads may still block the merge.\n\n## Body\n\npull request body\n\nDiscussion drill-down: pr://owner/repo/9/comments/<sel> (for example 3, 3-5, 3,7, or -1).\n\n")
    );

    assert!(aft.shutdown().success());
}

#[test]
fn github_outline_zoom_and_read_share_timeline_ordinals() {
    let temp = tempfile::tempdir().expect("create timeline fixture root");
    write_timeline_fake_gh(&temp.path().join("bin"));
    let mut aft = spawned_with_fake_gh(&temp.path().join("bin"));
    configure_gh_read_enabled(&mut aft, temp.path());
    let resource = "pr://cortexkit/aft/999";

    let outline = aft.send(
        &json!({
            "id": "timeline-outline",
            "command": "outline",
            "target": resource,
        })
        .to_string(),
    );
    assert_eq!(outline["success"], true, "outline failed: {outline:#}");
    let outline_text = outline["text"].as_str().expect("outline text");
    assert!(outline_text.contains("#999 Timeline fixture"));
    assert!(outline_text.contains("main<-timeline-fixture +3/-1 files=1 review=approved"));
    assert!(outline_text.contains("[8] event(closed) @aft-alfonso[bot] 2026-09-03 06:57"));
    assert!(outline_text.ends_with(&format!(
        "Zoom items: aft_zoom {resource} <k>[,k..] · full: read {resource}\n"
    )));

    let read = read_response(&mut aft, "timeline-read", resource, json!({}));
    assert_eq!(read["success"], true, "read failed: {read:#}");
    let read_text = read["content"].as_str().expect("read content");
    assert!(read_text.contains("## Timeline"));
    assert!(read_text.contains("### [8] @aft-alfonso[bot]"));
    assert!(read_text.contains("Event: closed"));
    assert_eq!(
        ordinals_with_prefix(outline_text, "["),
        ordinals_with_prefix(read_text, "### ["),
        "outline and read must address the same discussion items"
    );

    let zoom = aft.send(
        &json!({
            "id": "timeline-zoom",
            "command": "zoom",
            "file": resource,
            "symbols": ["8"],
        })
        .to_string(),
    );
    assert_eq!(zoom["success"], true, "zoom failed: {zoom:#}");
    assert!(zoom["content"]
        .as_str()
        .unwrap_or_default()
        .contains("Event: closed"));
    assert!(zoom["content"]
        .as_str()
        .unwrap_or_default()
        .contains("aft-alfonso[bot]"));

    let selected = read_response(
        &mut aft,
        "timeline-selector",
        &format!("{resource}/comments/8"),
        json!({}),
    );
    assert_eq!(selected["success"], true, "selector failed: {selected:#}");
    assert_eq!(selected["content"], zoom["content"]);

    let zoom_tail = aft.send(
        &json!({
            "id": "timeline-zoom-tail",
            "command": "zoom",
            "file": resource,
            "symbols": ["-1"],
        })
        .to_string(),
    );
    assert_eq!(
        zoom_tail["success"], true,
        "zoom tail failed: {zoom_tail:#}"
    );

    let selected_tail = read_response(
        &mut aft,
        "timeline-selector-tail",
        &format!("{resource}/comments/-1"),
        json!({}),
    );
    assert_eq!(
        selected_tail["success"], true,
        "selector tail failed: {selected_tail:#}"
    );
    assert_eq!(selected_tail["content"], zoom_tail["content"]);

    let selected_last_positive = read_response(
        &mut aft,
        "timeline-selector-last",
        &format!("{resource}/comments/9"),
        json!({}),
    );
    assert_eq!(selected_last_positive["success"], true);
    assert_eq!(selected_tail["content"], selected_last_positive["content"]);

    assert!(aft.shutdown().success());
}

#[test]
fn github_diff_outline_and_zoom_refuse_with_read_hint() {
    let project = tempfile::tempdir().expect("create diff refusal project");
    let mut aft = AftProcess::spawn();
    configure_gh_read_enabled(&mut aft, project.path());
    for target in ["pr://42/diff", "pr://owner/repo/42/diff/src/new.rs"] {
        for request in [
            json!({"id": "diff-outline", "command": "outline", "target": target}),
            json!({"id": "diff-zoom", "command": "zoom", "file": target, "symbols": ["1"]}),
        ] {
            let response = aft.send(&request.to_string());
            assert_eq!(response["success"], false, "{response:#}");
            assert!(
                response["message"]
                    .as_str()
                    .unwrap()
                    .contains("Use read for diffs"),
                "{response:#}"
            );
        }
    }
    assert!(aft.shutdown().success());
}

#[test]
fn github_diff_recorded_gh_reads_and_pages_through_protocol() {
    let project = tempfile::tempdir().expect("create diff fixture project");
    let bin = project.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    let metadata = include_str!("../../src/github_read/fixtures/pr-42-diff-metadata.json");
    // `gh api --paginate --jq` applies the filter to each page and prints one
    // compact JSON value per line, so the double answers with what the real
    // projection prints, not with the raw page.
    let files = serde_json::from_str::<Value>(metadata).unwrap()["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|file| json!({"path": file["path"], "previousPath": null}).to_string())
        .collect::<Vec<_>>()
        .join("\n");
    let patch = include_str!("../../src/github_read/fixtures/pr-42.diff");
    let script = format!(
        r#"#!/bin/sh
# Real gh refuses --slurp together with --jq or --template.
slurp=0; filter=0
for arg in "$@"; do
  case "$arg" in
    --slurp) slurp=1 ;;
    --jq|--template) filter=1 ;;
  esac
done
if [ $slurp = 1 ] && [ $filter = 1 ]; then
  echo 'the `--slurp` option is not supported with `--jq` or `--template`' >&2
  exit 1
fi
case "$1:$2" in
  pr:view) cat <<'JSON'
{metadata}
JSON
    ;;
  api:repos/owner/repo/pulls/42/files*) cat <<'JSON'
{files}
JSON
    ;;
  pr:diff) cat <<'PATCH'
{patch}
PATCH
    ;;
  *) echo 'unexpected fixture command' >&2; exit 1 ;;
esac
"#
    );
    link_cached_executable(&bin.join("gh"), script.as_bytes()).unwrap();
    let mut aft = spawned_with_fake_gh(&bin);
    configure_gh_read_enabled(&mut aft, project.path());
    let whole = read_response(&mut aft, "whole-diff", "pr://42/diff", json!({}));
    assert_eq!(whole["success"], true, "{whole:#}");
    assert!(whole["content"]
        .as_str()
        .unwrap()
        .starts_with("PR owner/repo#42 diff — head 0123456\n"));
    assert!(whole["content"]
        .as_str()
        .unwrap()
        .contains("assets/logo.png: binary file changed"));
    let page = read_response(
        &mut aft,
        "page-diff",
        "pr://42/diff",
        json!({"start_line": 7, "limit": 3}),
    );
    assert_eq!(page["success"], true, "{page:#}");
    assert_eq!(page["lines_read"], 3);
    assert_eq!(page["start_line"], 7);
    assert_eq!(page["end_line"], 9);
    assert_eq!(page["truncated"], true);
    assert!(page["content"]
        .as_str()
        .unwrap()
        .contains("head 0123456\n+new main\ndiff --git a/src/old.rs"));
    assert!(page["content"]
        .as_str()
        .unwrap()
        .contains("shown 3 of 18 lines (cap)"));
    let renamed = read_response(
        &mut aft,
        "renamed-diff",
        "pr://owner/repo/42/diff/src/new.rs",
        json!({}),
    );
    assert!(renamed["content"]
        .as_str()
        .unwrap()
        .contains("src/new.rs (renamed from src/old.rs)"));
    let unknown = read_response(
        &mut aft,
        "unknown-diff",
        "pr://42/diff/src/old.rs",
        json!({}),
    );
    assert_eq!(unknown["success"], false, "{unknown:#}");
    assert!(unknown["message"]
        .as_str()
        .unwrap()
        .contains("Refused: path is not in this PR"));
    assert!(aft.shutdown().success());
}

#[test]
fn github_outline_and_zoom_refuse_when_gh_read_is_disabled() {
    let project = tempfile::tempdir().expect("create gate project");
    let mut aft = AftProcess::spawn();
    let configured = aft.send(
        &json!({
            "id": "configure-gate-off",
            "command": "configure",
            "harness": "runner",
            "project_root": project.path(),
        })
        .to_string(),
    );
    assert_eq!(
        configured["success"], true,
        "configure failed: {configured:#}"
    );

    for request in [
        json!({ "id": "outline-gate-off", "command": "outline", "target": "issue://7" }),
        json!({ "id": "zoom-gate-off", "command": "zoom", "file": "issue://7", "symbols": ["1"] }),
    ] {
        let response = aft.send(&request.to_string());
        assert_eq!(
            response["success"], false,
            "gate unexpectedly passed: {response:#}"
        );
        assert_eq!(response["code"], "gh_read_disabled");
    }
    assert!(aft.shutdown().success());
}

#[test]
fn standalone_deferred_github_read_keeps_interleaved_request_responsive() {
    let temp = tempfile::tempdir().expect("create slow fake gh root");
    let bin_dir = temp.path().join("bin");
    write_fake_gh(&bin_dir);
    let fetch_started = temp.path().join("gh-fetch-started");
    let mut aft = spawned_with_slow_fake_gh(&bin_dir, &fetch_started);
    configure_gh_read_enabled(&mut aft, temp.path());

    aft.send_silent(
        &json!({
            "id": "slow-github-read",
            "command": "read",
            "file": "issue://7",
        })
        .to_string(),
    );
    let fetch_deadline = Instant::now() + Duration::from_secs(5);
    while !fetch_started.exists() {
        assert!(
            Instant::now() < fetch_deadline,
            "slow GitHub fetch did not start"
        );
        thread::sleep(Duration::from_millis(10));
    }

    let sibling = aft.send_with_timeout(
        &json!({
            "id": "interleaved-ping",
            "command": "ping",
        })
        .to_string(),
        Duration::from_millis(500),
    );
    assert_eq!(sibling["id"], "interleaved-ping");
    assert_eq!(sibling["success"], true);

    let github = loop {
        let frame = aft
            .try_read_next_timeout(Duration::from_secs(3))
            .expect("deferred GitHub read completes");
        if frame["id"] == "slow-github-read" {
            break frame;
        }
        assert!(
            frame.get("type").is_some() && frame.get("id").is_none(),
            "unexpected frame before deferred GitHub response: {frame:#}"
        );
    };
    assert_eq!(github["success"], true, "GitHub read failed: {github:#}");
    assert!(aft.shutdown().success());
}

#[test]
fn github_read_subc_translation_preserves_uri_selector_and_hidden_vision_capability() {
    let project = Path::new("/project");
    let with_vision = subc_translate_with_context(
        "read",
        &json!({
            "path": "issue://owner/repo/7",
            "offset": 3,
            "limit": 2,
            "vision_capability": true,
        }),
        project,
        TranslateContext::default(),
    )
    .expect("translate GitHub read with vision capability");
    assert_eq!(with_vision.command, "read");
    assert_eq!(with_vision.args["file"], "issue://owner/repo/7");
    assert_eq!(with_vision.args["start_line"], 3);
    assert_eq!(with_vision.args["end_line"], 4);
    assert_eq!(with_vision.args["vision_capability"], true);

    let without_vision = subc_translate_with_context(
        "read",
        &json!({ "path": "pr://9" }),
        project,
        TranslateContext::default(),
    )
    .expect("translate GitHub read without vision capability");
    assert!(
        without_vision.args.get("vision_capability").is_none(),
        "an absent hidden capability must remain absent for the read handler to treat as false"
    );
}

#[test]
fn github_read_returns_identical_selected_content_through_standalone_and_subc_paths() {
    let temp = tempfile::tempdir().expect("create fake gh root");
    write_fake_gh(&temp.path().join("bin"));
    let mut standalone = spawned_with_fake_gh(&temp.path().join("bin"));
    let mut subc = spawned_with_fake_gh(&temp.path().join("bin"));
    configure_gh_read_enabled(&mut standalone, temp.path());
    configure_gh_read_enabled(&mut subc, temp.path());

    let direct = read_response(
        &mut standalone,
        "github-selector-direct",
        "issue://7",
        json!({ "start_line": 3, "end_line": 4 }),
    );
    let translated = subc.send(
        &json!({
            "id": "github-selector-subc",
            "command": "tool_call",
            "session_id": "github-selector-session",
            "name": "read",
            "arguments": {
                "path": "issue://7",
                "offset": 3,
                "limit": 2,
            },
        })
        .to_string(),
    );

    assert_eq!(
        direct["success"], true,
        "standalone read failed: {direct:#}"
    );
    assert_eq!(
        translated["success"], true,
        "subc read failed: {translated:#}"
    );
    assert_eq!(direct["content"], "Repository: owner/repo\nState: OPEN\n");
    for field in [
        "content",
        "attachments",
        "total_lines",
        "lines_read",
        "start_line",
        "end_line",
        "truncated",
        "complete",
    ] {
        assert_eq!(
            direct[field], translated[field],
            "standalone/subc mismatch for {field}: standalone={direct:#} subc={translated:#}"
        );
    }

    assert!(standalone.shutdown().success());
    assert!(subc.shutdown().success());
}

#[test]
fn github_read_forced_restrict_refuses_before_any_filesystem_or_gh_work() {
    let project = tempfile::tempdir().expect("create restricted project");
    let ctx = AppContext::new(
        default_language_provider_factory(),
        crate::context_storage::isolate(Config {
            project_root: Some(project.path().to_path_buf()),
            ..Config::default()
        }),
    )
    .with_test_checkpoint_store();
    let request = RawRequest {
        id: "restricted-github-read".to_string(),
        command: "read".to_string(),
        lsp_hints: None,
        session_id: None,
        params: json!({ "file": "issue://7" }),
    };

    let request_id = request.id.clone();
    let outcome = ctx.with_force_restrict(&request_id, || build_read_outcome(request, &ctx));
    let DispatchOutcome::Immediate(response) = outcome else {
        panic!("restricted GitHub read must refuse synchronously");
    };
    assert!(!response.success);
    assert_eq!(response.data["code"], "external_fetch_restricted");
    assert!(
        response.data["message"]
            .as_str()
            .unwrap_or_default()
            .contains("Network-backed GitHub reads are unavailable on restricted binds"),
        "restricted response must explain the refusal: {:#}",
        response.data
    );
}

#[test]
fn github_zoom_batch_ordinals_fetch_each_discussion_once() {
    let temp = tempfile::tempdir().expect("create counted discussion fixture");
    let bin = temp.path().join("bin");
    let calls = temp.path().join("gh-calls");
    fs::create_dir_all(&bin).unwrap();
    link_cached_executable(
        &bin.join("gh"),
        br#"#!/bin/sh
printf '%s\n' "$1:$2" >> "$AFT_TEST_GH_CALL_LOG"
case "$1:$2" in
  issue:view|pr:view)
    printf '%s\n' '{"number":410,"title":"Counted discussion","state":"OPEN","body":"Fixture body","url":"https://github.com/owner/repo/issues/410","comments":[{"author":{"login":"first"},"body":"First selected comment","createdAt":"2026-09-03T05:10:00Z"},{"author":{"login":"second"},"body":"Second selected comment","createdAt":"2026-09-03T05:20:00Z"}],"files":[],"reviews":[]}'
    ;;
  api:graphql)
    printf '%s\n' '{"data":{"repository":{"nameWithOwner":"owner/repo","pullRequest":{"number":410,"title":"Counted discussion","state":"OPEN","body":"Fixture body","comments":[],"files":[],"reviews":[]}}}}'
    ;;
  api:repos/*)
    printf '%s\n' '[[]]'
    ;;
  *) exit 1 ;;
esac
"#,
    )
    .expect("cache counted gh fixture");
    let original_path = std::env::var_os("PATH").unwrap_or_default();
    let path =
        std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(&original_path)))
            .unwrap();
    let mut aft = AftProcess::spawn_with_env(&[
        ("PATH", path.as_os_str()),
        ("AFT_TEST_GH_CALL_LOG", calls.as_os_str()),
    ]);
    configure_gh_read_enabled(&mut aft, temp.path());
    for target in ["issue://410", "pr://owner/repo/410"] {
        for arguments in [
            json!({ "batch": [{ "target": target, "symbols": ["1", "2"] }] }),
            json!({ "path": target, "symbols": ["1", "2"] }),
            json!({ "targets": [{ "path": target, "symbols": ["1", "2"] }] }),
        ] {
            // Each discussion fetch starts with one structured view request;
            // count that request rather than its additional timeline/review calls.
            let view_count = || {
                fs::read_to_string(&calls)
                    .unwrap_or_default()
                    .lines()
                    .filter(|line| matches!(*line, "issue:view" | "pr:view"))
                    .count()
            };
            let before = view_count();
            let response = aft.send(
                &json!({ "id": "counted-zoom", "command": "tool_call", "session_id": "github-zoom-fetch-count", "name": "zoom", "arguments": arguments })
                    .to_string(),
            );
            assert_eq!(response["success"], true, "{response:#}");
            let entries = response["targets"].as_array().expect("zoom targets");
            for entry in entries {
                assert_eq!(entry["response"]["success"], true, "{response:#}");
            }
            let text = response["text"].as_str().unwrap_or_default();
            assert!(text.contains("First selected comment"), "{response:#}");
            assert!(text.contains("Second selected comment"), "{response:#}");
            assert_eq!(
                view_count() - before,
                1,
                "one fetch per GitHub batch entry: {response:#}"
            );
            assert_eq!(
                entries.len(),
                1,
                "one combined selector per entry: {response:#}"
            );
            assert_eq!(entries[0]["name"], "1,2", "{response:#}");
        }
    }
    assert!(aft.shutdown().success());
}
