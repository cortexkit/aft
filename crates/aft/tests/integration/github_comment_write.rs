#![cfg(unix)]

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::json;

use super::helpers::{link_cached_executable, AftProcess};

fn write_fake_gh(path: &Path) {
    link_cached_executable(
        path,
        br#"#!/bin/sh
if [ "$1 $2" = "gh-shim --shim-version" ]; then
  printf '%s\n' '{"shim_version":"fixture","gh_routing_schema_floor":1}'
  exit 0
fi
if [ "$1 $2" = "issue comment" ]; then
  printf '%s\n' "$@" > "$AFT_GITHUB_WRITE_ARGV"
  cat > "$AFT_GITHUB_WRITE_STDIN"
  if [ "${AFT_GITHUB_WRITE_REFUSE:-}" = "1" ]; then
    printf '%s\n' 'fixture shim policy refused this comment' >&2
    exit 86
  fi
  printf '%s\n' 'https://github.com/owner/repo/issues/7#issuecomment-901'
  exit 0
fi
if [ "$1 $2" = "issue view" ]; then
  printf '%s\n' '{"number":7,"title":"Fixture issue","state":"OPEN","body":"body","url":"https://github.com/owner/repo/issues/7","comments":[{"author":{"login":"aft-bot"},"body":"exact body with stale target","createdAt":"2026-09-10T12:00:00Z","updatedAt":"2026-09-10T12:00:00Z","url":"https://github.com/owner/repo/issues/7#issuecomment-901"}]}'
  exit 0
fi
if [ "$1 $2" = "api --method" ]; then
  printf '%s\n' "$@" > "$AFT_GITHUB_EDIT_ARGV"
  cat > "$AFT_GITHUB_EDIT_STDIN"
  if [ "${AFT_GITHUB_WRITE_REFUSE:-}" = "1" ]; then
    printf '%s\n' 'fixture shim says bot cannot edit this comment' >&2
    exit 86
  fi
  printf '%s\n' '{}'
  exit 0
fi
if [ "$1" = "api" ]; then
  printf '%s\n' '[]'
  exit 0
fi
printf '%s\n' "unexpected fake gh invocation: $*" >&2
exit 2
"#,
    )
    .expect("cache fake gh");
}

fn path_with(bin: &Path) -> std::ffi::OsString {
    env::join_paths(
        std::iter::once(bin.to_path_buf())
            .chain(env::split_paths(&env::var_os("PATH").unwrap_or_default())),
    )
    .expect("join fake gh PATH")
}

struct Fixture {
    _root: tempfile::TempDir,
    project: PathBuf,
    user_config: PathBuf,
    argv: PathBuf,
    stdin: PathBuf,
    edit_argv: PathBuf,
    edit_stdin: PathBuf,
    state: PathBuf,
    home: PathBuf,
    xdg_config: PathBuf,
    xdg_data: PathBuf,
    xdg_cache: PathBuf,
    path: std::ffi::OsString,
}

impl Fixture {
    fn new(github: serde_json::Value) -> Self {
        let root = tempfile::tempdir().expect("create GitHub write fixture");
        let github = if github.get("enabled").is_some() {
            // Write the resolved `github.read`, `github.write` and `github.shim`
            // values instead of the retired `github.enabled`, so the child reads
            // a current config file and does not rewrite it.
            let tier = aft::config_resolve::ConfigTier {
                tier: "user".to_string(),
                source: "fixture".to_string(),
                doc: json!({ "github": github }).to_string(),
            };
            let resolved = aft::config_resolve::resolve_config_for_harness(&[tier], None);
            assert!(
                resolved.errors.is_empty(),
                "legacy GitHub config: {:?}",
                resolved.errors
            );
            serde_json::to_value(resolved.config.github).expect("serialize resolved GitHub config")
        } else {
            github
        };
        let project = root.path().join("project");
        let bin = root.path().join("bin");
        let fake_gh = bin.join("fake-aft-shim");
        let user_config = root.path().join("user-aft.jsonc");
        fs::create_dir_all(&project).expect("create project");
        fs::create_dir_all(&bin).expect("create bin");
        write_fake_gh(&fake_gh);
        std::os::unix::fs::symlink(&fake_gh, bin.join("gh")).expect("link fake gh on PATH");
        fs::write(
            &user_config,
            serde_json::to_vec(&json!({
                "github": github,
                "gh_shim": { "binary_path": fake_gh },
                // Every mutating tool must reach its GitHub-address refusal,
                // including `aft_delete` and `aft_move`, which the default
                // config disables and dispatch would otherwise refuse first.
                "disabled_tools": [],
            }))
            .expect("serialize config"),
        )
        .expect("write user config");
        Self {
            argv: root.path().join("argv.log"),
            stdin: root.path().join("stdin.log"),
            edit_argv: root.path().join("edit-argv.log"),
            edit_stdin: root.path().join("edit-stdin.log"),
            state: root.path().join("shim-state"),
            home: root.path().join("home"),
            xdg_config: root.path().join("xdg-config"),
            xdg_data: root.path().join("xdg-data"),
            xdg_cache: root.path().join("xdg-cache"),
            path: path_with(&bin),
            project,
            user_config,
            _root: root,
        }
    }

    fn spawn(&self, refuse: bool) -> AftProcess {
        let mut envs = vec![
            ("PATH", self.path.as_os_str()),
            ("HOME", self.home.as_os_str()),
            ("XDG_CONFIG_HOME", self.xdg_config.as_os_str()),
            ("XDG_DATA_HOME", self.xdg_data.as_os_str()),
            ("XDG_CACHE_HOME", self.xdg_cache.as_os_str()),
            ("AFT_GH_SHIM_STATE_DIR", self.state.as_os_str()),
            ("AFT_GITHUB_WRITE_ARGV", self.argv.as_os_str()),
            ("AFT_GITHUB_WRITE_STDIN", self.stdin.as_os_str()),
            ("AFT_GITHUB_EDIT_ARGV", self.edit_argv.as_os_str()),
            ("AFT_GITHUB_EDIT_STDIN", self.edit_stdin.as_os_str()),
        ];
        if refuse {
            envs.push(("AFT_GITHUB_WRITE_REFUSE", std::ffi::OsStr::new("1")));
        }
        let mut aft = AftProcess::spawn_with_env(&envs);
        let configured = aft.send(
            &json!({
                "id": "configure-github-write",
                "command": "configure",
                "harness": "runner",
                "project_root": self.project,
                "cortexkit_user_config_path": self.user_config,
            })
            .to_string(),
        );
        assert_eq!(
            configured["success"], true,
            "configure failed: {configured:#}"
        );
        aft
    }
}

/// Every file and directory under `root`, relative and sorted, so a test can
/// prove a refused call left the project byte-for-byte untouched in shape.
fn project_entries(root: &Path) -> Vec<String> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let relative = path
                .strip_prefix(root)
                .expect("entry under root")
                .to_string_lossy()
                .into_owned();
            let contents = fs::read(&path).map(|bytes| bytes.len()).unwrap_or(0);
            out.push(format!("{relative} ({contents} bytes)"));
            if path.is_dir() {
                walk(root, &path, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

fn tool_call(
    aft: &mut AftProcess,
    id: &str,
    name: &str,
    arguments: serde_json::Value,
) -> serde_json::Value {
    aft.send(
        &json!({
            "id": id,
            "command": "tool_call",
            "session_id": "github-write-session",
            "name": name,
            "arguments": arguments,
        })
        .to_string(),
    )
}

/// The agent-facing tool surface (the `tool_call` translation shared by the
/// plugin bridge and subc routes) once resolved `issue://369` against the
/// project root and wrote `<project>/issue:/369`. With github.write off, every
/// mutating tool that takes a path must refuse a GitHub address and leave the
/// project exactly as it was.
#[test]
fn mutating_tools_refuse_github_addresses_without_touching_the_project_when_write_is_off() {
    let fixture = Fixture::new(json!({ "write": false }));
    fs::write(fixture.project.join("seed.txt"), "seed\n").expect("seed project file");
    let mut aft = fixture.spawn(false);
    let before = project_entries(&fixture.project);

    // (label, tool, arguments, required error code when the refusal must name
    // the github.write setting)
    let cases: Vec<(&str, &str, serde_json::Value, Option<&str>)> = vec![
        (
            "write issue",
            "write",
            json!({ "path": "issue://369", "content": "comment body" }),
            Some("github_write_disabled"),
        ),
        (
            "write pr via filePath",
            "write",
            json!({ "filePath": "pr://owner/repo/12", "content": "comment body" }),
            Some("github_write_disabled"),
        ),
        (
            "edit comment find/replace",
            "edit",
            json!({
                "path": "issue://7/comments/1",
                "edits": [{ "oldString": "stale target", "newString": "fresh target" }],
            }),
            Some("github_write_disabled"),
        ),
        (
            "edit append",
            "edit",
            json!({ "path": "issue://7", "appendContent": "more" }),
            None,
        ),
        (
            "edit symbol",
            "edit",
            json!({ "path": "issue://7", "symbol": "main", "content": "fn main() {}" }),
            None,
        ),
        (
            "edit line range",
            "edit",
            json!({
                "path": "pr://3",
                "edits": [{ "startLine": 1, "endLine": 1, "content": "x" }],
            }),
            None,
        ),
        (
            "edit two find/replace entries",
            "edit",
            json!({
                "path": "issue://7/comments/1",
                "edits": [
                    { "oldString": "a", "newString": "b" },
                    { "oldString": "c", "newString": "d" },
                ],
            }),
            None,
        ),
        (
            "apply_patch add",
            "apply_patch",
            json!({ "patchText": "*** Begin Patch\n*** Add File: issue://7\n+body\n*** End Patch" }),
            None,
        ),
        (
            "apply_patch move destination",
            "apply_patch",
            json!({ "patchText": "*** Begin Patch\n*** Update File: seed.txt\n*** Move to: pr://3\n@@\n-seed\n+seed moved\n*** End Patch" }),
            None,
        ),
        ("delete", "delete", json!({ "files": ["issue://7"] }), None),
        (
            "move destination",
            "move",
            json!({ "path": "seed.txt", "destination": "issue://7" }),
            None,
        ),
        (
            "move source",
            "move",
            json!({ "path": "pr://3", "destination": "moved.txt" }),
            None,
        ),
        (
            "import add",
            "import",
            json!({ "op": "add", "path": "issue://7", "module": "std::fmt" }),
            None,
        ),
        (
            "safety checkpoint",
            "safety",
            json!({ "op": "checkpoint", "name": "gh", "path": "issue://7" }),
            None,
        ),
        (
            "safety restore",
            "safety",
            json!({ "op": "restore", "name": "gh", "files": ["issue://7"] }),
            None,
        ),
        (
            "safety undo",
            "safety",
            json!({ "op": "undo", "path": "issue://7" }),
            None,
        ),
        (
            "ast_replace",
            "ast_replace",
            json!({
                "pattern": "foo($A)",
                "rewrite": "bar($A)",
                "lang": "rust",
                "paths": ["issue://7"],
            }),
            None,
        ),
    ];

    for (index, (label, tool, arguments, code)) in cases.into_iter().enumerate() {
        let response = tool_call(&mut aft, &format!("gh-refuse-{index}"), tool, arguments);
        assert_eq!(
            response["success"], false,
            "{label}: a GitHub address must be refused: {response:#}"
        );
        if let Some(code) = code {
            assert_eq!(response["code"], code, "{label}: {response:#}");
            assert!(
                response.to_string().contains("github.write"),
                "{label}: refusal must name the setting: {response:#}"
            );
        }
        assert!(
            response["message"].as_str().unwrap_or_default().contains("GitHub"),
            "{label}: refusal must say the address is a GitHub one, not fail incidentally: {response:#}"
        );
        assert_eq!(
            project_entries(&fixture.project),
            before,
            "{label}: refused call changed the project directory"
        );
    }
    assert!(!fixture.argv.exists(), "a refused write reached gh");
    assert!(!fixture.edit_argv.exists(), "a refused edit reached gh");
    assert!(aft.shutdown().success());
}

/// Native protocol commands are a second way in. The raw `issue://` spelling
/// must never reach the filesystem through any of them either.
#[test]
fn native_mutation_commands_refuse_github_addresses_without_touching_the_project() {
    let fixture = Fixture::new(json!({ "write": false }));
    fs::write(fixture.project.join("seed.txt"), "seed\n").expect("seed project file");
    let mut aft = fixture.spawn(false);
    let before = project_entries(&fixture.project);
    let seed = fixture.project.join("seed.txt");

    let cases = vec![
        json!({ "command": "write", "file": "issue://369", "content": "body" }),
        json!({
            "command": "batch",
            "file": "issue://7",
            "edits": [{ "line_start": 1, "line_end": 1, "content": "x" }],
        }),
        json!({
            "command": "edit_match",
            "file": "issue://7",
            "op": "append",
            "append_content": "x",
        }),
        json!({ "command": "delete_file", "files": ["issue://7"] }),
        json!({ "command": "move_file", "file": seed, "destination": "issue://7" }),
        json!({ "command": "add_import", "file": "issue://7", "module": "std::fmt" }),
        json!({
            "command": "apply_patch",
            "patch_text": "*** Begin Patch\n*** Add File: pr://3\n+body\n*** End Patch",
        }),
        json!({ "command": "checkpoint", "name": "gh", "files": ["issue://7"] }),
        json!({ "command": "undo", "file": "pr://3" }),
    ];
    for (index, mut request) in cases.into_iter().enumerate() {
        request["id"] = json!(format!("native-gh-refuse-{index}"));
        let response = aft.send(&request.to_string());
        assert_eq!(
            response["success"], false,
            "native {}: a GitHub address must be refused: {response:#}",
            request["command"]
        );
        assert!(
            response["message"].as_str().unwrap_or_default().contains("GitHub"),
            "native {}: refusal must say the address is a GitHub one, not fail incidentally: {response:#}",
            request["command"]
        );
        assert_eq!(
            project_entries(&fixture.project),
            before,
            "native {}: refused call changed the project directory",
            request["command"]
        );
    }
    assert!(!fixture.argv.exists(), "a refused native write reached gh");
    assert!(aft.shutdown().success());
}

/// With github.write on, the agent-facing `write` tool posts the comment and
/// returns its URL instead of writing a local file. `tool_call` is the path the
/// plugin bridge and every subc route take (both call `prepare_tool_call`).
#[test]
fn write_tool_call_posts_comment_and_returns_url_when_write_is_on() {
    let fixture = Fixture::new(json!({ "write": true }));
    let mut aft = fixture.spawn(false);
    let before = project_entries(&fixture.project);

    let response = tool_call(
        &mut aft,
        "github-write-tool-call",
        "write",
        json!({ "filePath": "issue://owner/repo/7", "content": "tool call body" }),
    );

    assert_eq!(response["success"], true, "write failed: {response:#}");
    assert_eq!(
        response["comment_url"],
        "https://github.com/owner/repo/issues/7#issuecomment-901"
    );
    assert!(
        response["text"]
            .as_str()
            .expect("response text")
            .contains("https://github.com/owner/repo/issues/7#issuecomment-901"),
        "the agent-visible text must carry the comment URL: {response:#}"
    );
    assert_eq!(
        fs::read_to_string(&fixture.stdin).expect("read stdin"),
        "tool call body"
    );
    assert_eq!(
        fs::read_to_string(&fixture.argv).expect("read argv"),
        "issue\ncomment\n7\n-R\nowner/repo\n--body-file\n-\n"
    );
    assert_eq!(project_entries(&fixture.project), before);
    assert!(aft.shutdown().success());
}

#[test]
fn edit_tool_call_patches_the_addressed_comment_when_write_is_on() {
    let fixture = Fixture::new(json!({ "write": true, "read": true }));
    let mut aft = fixture.spawn(false);
    let before = project_entries(&fixture.project);

    let response = tool_call(
        &mut aft,
        "github-edit-tool-call",
        "edit",
        json!({
            "path": "issue://owner/repo/7/comments/1",
            "edits": [{ "oldString": "stale target", "newString": "fresh target" }],
        }),
    );

    assert_eq!(response["success"], true, "edit failed: {response:#}");
    assert!(
        response["text"]
            .as_str()
            .expect("response text")
            .contains("https://github.com/owner/repo/issues/7#issuecomment-901"),
        "the agent-visible text must carry the comment URL: {response:#}"
    );
    assert_eq!(
        response["comment_url"],
        "https://github.com/owner/repo/issues/7#issuecomment-901"
    );
    assert_eq!(
        fs::read_to_string(&fixture.edit_stdin).expect("read edit stdin"),
        r#"{"body":"exact body with fresh target"}"#
    );
    assert_eq!(project_entries(&fixture.project), before);
    assert!(aft.shutdown().success());
}

#[test]
fn write_posts_exact_stdin_through_managed_shim_and_reports_live_ordinal() {
    let fixture = Fixture::new(json!({ "write": true }));
    let mut aft = fixture.spawn(false);
    let response = aft.send(
        &json!({
            "id": "github-write",
            "command": "write",
            "file": "issue://owner/repo/7",
            "content": "exact body",
        })
        .to_string(),
    );

    assert_eq!(response["success"], true, "write failed: {response:#}");
    assert_eq!(response["ordinal"], 1);
    assert_eq!(
        response["comment_url"],
        "https://github.com/owner/repo/issues/7#issuecomment-901"
    );
    assert!(response["text"]
        .as_str()
        .expect("response text")
        .contains("Comments cannot be undone with aft_safety"));
    assert_eq!(
        fs::read_to_string(&fixture.stdin).expect("read stdin"),
        "exact body"
    );
    assert_eq!(
        fs::read_to_string(&fixture.argv).expect("read argv"),
        "issue\ncomment\n7\n-R\nowner/repo\n--body-file\n-\n"
    );
    assert!(aft.shutdown().success());
}

#[test]
fn write_surfaces_exit_86_as_typed_shim_refusal() {
    let fixture = Fixture::new(json!({ "write": true }));
    let mut aft = fixture.spawn(true);
    let response = aft.send(
        &json!({
            "id": "github-write-refused",
            "command": "write",
            "file": "issue://7",
            "content": "blocked body",
        })
        .to_string(),
    );

    assert_eq!(response["success"], false);
    assert_eq!(response["code"], "gh_shim_refused");
    assert!(response["message"]
        .as_str()
        .unwrap_or_default()
        .contains("fixture shim policy refused this comment"));
    assert!(aft.shutdown().success());
}

#[test]
fn write_gate_refuses_before_any_gh_traffic() {
    for github in [json!({ "write": false })] {
        let fixture = Fixture::new(github);
        let mut aft = fixture.spawn(false);
        let response = aft.send(
            &json!({
                "id": "github-write-off",
                "command": "write",
                "file": "issue://7",
                "content": "must not post",
            })
            .to_string(),
        );
        assert_eq!(response["success"], false);
        assert_eq!(response["code"], "github_write_disabled");
        assert!(response.to_string().contains("github.write"));
        assert!(!fixture.argv.exists(), "disabled write reached gh");
        assert!(!fixture.stdin.exists(), "disabled write sent a body");
        assert!(aft.shutdown().success());
    }
}

#[test]
fn edit_fetches_matches_and_patches_the_selected_comment_id() {
    let fixture = Fixture::new(json!({ "write": true, "read": true }));
    let mut aft = fixture.spawn(false);
    let response = aft.send(
        &json!({
            "id": "github-edit",
            "command": "edit_match",
            "file": "issue://owner/repo/7/comments/1",
            "match": "stale target",
            "replacement": "fresh target",
        })
        .to_string(),
    );

    assert_eq!(response["success"], true, "edit failed: {response:#}");
    assert_eq!(response["ordinal"], 1);
    assert_eq!(response["replacements"], 1);
    assert_eq!(
        fs::read_to_string(&fixture.edit_argv).expect("read edit argv"),
        "api\n--method\nPATCH\nrepos/owner/repo/issues/comments/901\n--input\n-\n"
    );
    assert_eq!(
        fs::read_to_string(&fixture.edit_stdin).expect("read edit stdin"),
        r#"{"body":"exact body with fresh target"}"#
    );
    assert!(aft.shutdown().success());
}

#[test]
fn edit_stale_text_fails_at_match_time_without_posting() {
    let fixture = Fixture::new(json!({ "write": true, "read": true }));
    let mut aft = fixture.spawn(false);
    let response = aft.send(
        &json!({
            "id": "github-edit-stale",
            "command": "edit_match",
            "file": "issue://7/comments/1",
            "match": "already changed elsewhere",
            "replacement": "must not clobber",
        })
        .to_string(),
    );

    assert_eq!(response["success"], false);
    assert_eq!(response["code"], "match_not_found");
    assert!(!fixture.edit_argv.exists(), "stale edit reached gh PATCH");
    assert!(!fixture.edit_stdin.exists(), "stale edit sent a body");
    assert!(aft.shutdown().success());
}

#[test]
fn edit_surfaces_bot_ownership_refusal_from_the_shim() {
    let fixture = Fixture::new(json!({ "write": true, "read": true }));
    let mut aft = fixture.spawn(true);
    let response = aft.send(
        &json!({
            "id": "github-edit-refused",
            "command": "edit_match",
            "file": "issue://7/comments/1",
            "match": "stale target",
            "replacement": "fresh target",
        })
        .to_string(),
    );

    assert_eq!(response["success"], false);
    assert_eq!(response["code"], "gh_shim_refused");
    assert!(response["message"]
        .as_str()
        .unwrap_or_default()
        .contains("bot cannot edit this comment"));
    assert!(aft.shutdown().success());
}
