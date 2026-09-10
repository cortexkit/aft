#![cfg(unix)]

use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde_json::json;

use super::helpers::AftProcess;

fn write_fake_gh(path: &Path) {
    fs::write(
        path,
        r#"#!/bin/sh
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
    .expect("write fake gh");
    let mut permissions = fs::metadata(path).expect("stat fake gh").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("make fake gh executable");
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
fn write_gate_and_master_off_refuse_before_any_gh_traffic() {
    for github in [
        json!({ "write": false }),
        json!({
            "enabled": false,
            "write": true,
            "read": true,
            "shim": true
        }),
    ] {
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
