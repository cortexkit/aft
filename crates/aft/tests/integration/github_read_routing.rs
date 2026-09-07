#![cfg(unix)]

use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::PermissionsExt;
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

use super::helpers::AftProcess;

fn write_fake_gh(bin_dir: &Path) {
    fs::create_dir_all(bin_dir).expect("create fake gh directory");
    let script = bin_dir.join("gh");
    fs::write(
        &script,
        r#"#!/bin/sh
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
    .expect("write fake gh");
    let mut permissions = fs::metadata(&script).expect("stat fake gh").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&script, permissions).expect("make fake gh executable");
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
    fs::write(&user_config, "{\"gh_read\": {\"enabled\": true}}")
        .expect("write gh_read user config");
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
    fs::write(
        &script,
        r#"#!/bin/sh
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
    .expect("write timeline fixture gh script");
    let mut permissions = fs::metadata(&script)
        .expect("stat timeline fixture gh script")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&script, permissions).expect("make timeline fixture gh script executable");
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
    assert_eq!(issue["content"].as_str(), Some("# Issue #7: Fixture issue\n\nRepository: owner/repo\nState: OPEN\n\n## Body\n\none\ntwo\nthree\nhttps://github.com/user-attachments/files/7/screenshot.png\n\nDiscussion drill-down: issue://7/comments/<sel> (for example 3, 3-5, or 3,7).\n\n"));
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
        Some("# Pull request #9: Fixture pull request\n\nRepository: owner/repo\nState: OPEN\n\n## Body\n\npull request body\n\nDiscussion drill-down: pr://owner/repo/9/comments/<sel> (for example 3, 3-5, or 3,7).\n\n")
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
        Config {
            project_root: Some(project.path().to_path_buf()),
            ..Config::default()
        },
    );
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
