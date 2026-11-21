use crate::config::Config;
use crate::config_live::*;
use crate::context::AppContext;
use crate::parser::TreeSitterProvider;
use crate::protocol::RawRequest;
use aft_engine::test_bridge::display_data::test_config_file::write;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

struct Fixture {
    ctx: AppContext,
    root: PathBuf,
    user_path: PathBuf,
    project_path: PathBuf,
    _temp: tempfile::TempDir,
}

impl Fixture {
    fn new(user: &str, project: Option<&str>) -> Self {
        let temp = tempfile::tempdir().expect("temp dir");
        let root = temp.path().join("project");
        std::fs::create_dir_all(&root).expect("project root");
        let user_path = temp.path().join("xdg/cortexkit/aft.jsonc");
        let project_path = project_config_path(&root);
        write(&user_path, user);
        if let Some(project) = project {
            write(&project_path, project);
        }
        let ctx = crate::test_context(
            Box::new(TreeSitterProvider::new()),
            Config {
                storage_dir: Some(temp.path().join("storage")),
                ..Config::default()
            },
        );
        let fixture = Self {
            ctx,
            root,
            user_path,
            project_path,
            _temp: temp,
        };
        fixture.configure();
        fixture
    }

    fn configure(&self) {
        let req: RawRequest = serde_json::from_value(json!({
            "id": "configure-live-reload",
            "command": "configure",
            "project_root": self.root,
            "harness": "opencode",
            "cortexkit_user_config_path": self.user_path,
        }))
        .expect("configure request");
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let response = crate::commands::configure::handle_configure(&req, &self.ctx);
        assert!(response.success, "configure failed: {:?}", response.data);
    }

    /// Run the reload the file watcher would have asked for.
    ///
    /// Runs it directly rather than through the debounced signal: a file
    /// watch another test turned on for the process may push the signal's
    /// due time out while this test writes the file.
    fn reload(&self) -> ReloadOutcome {
        reload_config_now(&self.ctx)
    }
}

#[test]
fn staged_sources_are_cleared_at_entry_and_on_unwind() {
    // Observe staging directly rather than a later configure's successful overwrite.
    let fixture = Fixture::new("{}", None);
    let sources = fixture.ctx.config_live().sources().unwrap();
    stage_configure_sources(sources.clone());
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _scope = configure_sources_scope();
        assert!(
            staged_sources_empty_for_test(),
            "new configure must discard old staging"
        );
        stage_configure_sources(sources);
        panic!("configure after staging");
    }));
    assert!(
        staged_sources_empty_for_test(),
        "unwind must discard staging"
    );
}

#[test]
fn macos_full_sync_tracks_configure_and_reload_not_context_construction() {
    let name = concat!(
        module_path!(),
        "::macos_full_sync_tracks_configure_and_reload_not_context_construction"
    )
    .trim_start_matches("aft::");
    const CHILD: &str = "AFT_TEST_MACOS_FULL_SYNC_CHILD";
    // Publications from unrelated tests must not race this process-wide policy.
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture", "--color=never"])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success()
                && String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"),
            "sync policy child failed: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    assert!(!aft_store::sync::macos_full_sync());
    let fixture = Fixture::new(
        r#"{"durability":{"macos_full_sync":true},"harnesses":{"opencode":{"durability":{"macos_full_sync":false}}}}"#,
        None,
    );
    assert!(aft_store::sync::macos_full_sync());
    let _second = crate::test_context(Box::new(TreeSitterProvider::new()), Config::default());
    assert!(aft_store::sync::macos_full_sync());
    for enabled in [false, true] {
        write(
            &fixture.user_path,
            &format!("{{\"durability\":{{\"macos_full_sync\":{enabled}}}}}"),
        );
        let outcome = fixture.reload();
        assert!(applied(&outcome).contains(&"durability.macos_full_sync"));
        assert_eq!(aft_store::sync::macos_full_sync(), enabled);
        assert_eq!(
            aft_store::sync::mode(),
            if cfg!(target_os = "macos") && enabled {
                "full_sync"
            } else {
                "fsync"
            }
        );
    }
    write(&fixture.user_path, "{}");
    assert!(applied(&fixture.reload()).contains(&"durability.macos_full_sync"));
    assert!(!aft_store::sync::macos_full_sync());
}

#[test]
fn prepared_bind_watches_keep_the_captured_source() {
    let name = concat!(
        module_path!(),
        "::prepared_bind_watches_keep_the_captured_source"
    )
    .trim_start_matches("aft::");
    const CHILD: &str = "AFT_TEST_PREPARED_WATCH_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture", "--color=never"])
            .env(CHILD, "1")
            .env_remove("AFT_TEST_DISABLE_FILE_WATCHER")
            .output()
            .unwrap();
        assert!(
            output.status.success()
                && String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"),
            "watch child failed: {:?} {:?}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let _git_env = crate::test_env::hermetic_git_env_guard();
    let fixture = Fixture::new(
        r#"{"indexes":{"trigram":false,"semantic":false,"callgraph":false}}"#,
        None,
    );
    let legacy = fixture.root.join(".pi/aft.jsonc");
    write(&legacy, r#"{"idle": {"checkout_hours": 7}}"#);
    let prepared = crate::subc_config::PreparedBindConfig::read(
        Some(&fixture.user_path),
        &fixture.root,
        &crate::harness::Harness::Pi,
    )
    .unwrap();
    write(&fixture.project_path, r#"{"idle": {"checkout_hours": 9}}"#);
    std::fs::remove_file(&legacy).unwrap();
    enable_config_watches_for_test();
    let request: RawRequest = serde_json::from_value(json!({"id":"prepared-watch","command":"configure","project_root":fixture.root,"harness":"pi","cortexkit_user_config_path":fixture.user_path})).unwrap();
    let response = crate::subc_config::with_prepared_bind_config(prepared, || {
        crate::commands::configure::handle_configure(&request, &fixture.ctx)
    });
    assert!(response.success, "{}", response.data);
    fixture.ctx.config_live().wait_for_watch_attachment();
    assert_eq!(
        fixture
            .ctx
            .config_live()
            .project_fallback_file_for_test()
            .as_deref()
            .unwrap(),
        legacy.as_path()
    );
}

#[test]
fn inspect_categories_and_lsp_idle_reload_without_restart() {
    let fixture = Fixture::new(r#"{"lsp":{"idle_minutes":60,"max_servers":8}}"#, None);
    write(
        &fixture.user_path,
        r#"{"lsp":{"idle_minutes":"never","max_servers":3},"inspect":{"categories":{"dead_code":false,"todos":false}}}"#,
    );
    let outcome = fixture.reload();
    assert!(applied(&outcome).contains(&"lsp.idle_shutdown"));
    assert!(applied(&outcome).contains(&"lsp.max_servers"));
    assert_eq!(fixture.ctx.config().lsp_max_servers, 3);
    assert_eq!(fixture.ctx.config().lsp_user_max_servers, Some(3));
    assert!(applied(&outcome).contains(&"inspect.categories"));
    assert!(deferred(&outcome).is_empty());
    assert!(!fixture.ctx.config().lsp_idle_shutdown);
    fixture.ctx.update_status_bar_todos(Some(4));
    let counts = fixture.ctx.status_bar_count_values();
    assert!(counts.disabled_categories.contains(&"dead_code"));
    assert_eq!(counts.todos, None);
    write(
        &fixture.user_path,
        r#"{"lsp":{"idle_minutes":5},"inspect":{"categories":{"dead_code":true}}}"#,
    );
    let outcome = fixture.reload();
    assert!(applied(&outcome).contains(&"lsp.idle_minutes"));
    assert_eq!(fixture.ctx.config().lsp_idle_minutes, 5);
    assert!(fixture.ctx.config().lsp_idle_shutdown);
    assert!(fixture.ctx.config().inspect.categories.dead_code);
    assert!(!fixture
        .ctx
        .status_bar_count_values()
        .disabled_categories
        .contains(&"dead_code"));
}

fn applied(outcome: &ReloadOutcome) -> Vec<&'static str> {
    match outcome {
        ReloadOutcome::Reloaded { applied, .. } => applied.clone(),
        other => panic!("expected a reload, got {other:?}"),
    }
}

fn deferred(outcome: &ReloadOutcome) -> Vec<&'static str> {
    match outcome {
        ReloadOutcome::Reloaded { deferred, .. } => deferred.clone(),
        other => panic!("expected a reload, got {other:?}"),
    }
}

#[test]
fn live_edit_applies_a_group_a_key_without_reconnect() {
    let fixture = Fixture::new("{}", Some(r#"{ "bash": { "enabled": true } }"#));
    assert!(fixture.ctx.config().bash.enabled);
    let generation = fixture.ctx.configure_generation();

    write(
        &fixture.project_path,
        r#"{ "bash": { "enabled": false }, "format_on_edit": true }"#,
    );
    let outcome = fixture.reload();

    assert_eq!(applied(&outcome), vec!["format_on_edit", "bash.enabled"]);
    assert!(!fixture.ctx.config().bash.enabled);
    assert!(fixture.ctx.config().format_on_edit);
    // No configure ran: the configure generation is unchanged.
    assert_eq!(fixture.ctx.configure_generation(), generation);
}

#[test]
fn bash_env_updates_live_for_future_commands() {
    let fixture = Fixture::new(r#"{ "bash": { "env": { "TZ": "UTC" } } }"#, None);
    write(
        &fixture.user_path,
        r#"{ "bash": { "env": { "TZ": "Etc/GMT", "LANG": "C.UTF-8" } } }"#,
    );

    let outcome = fixture.reload();
    assert_eq!(applied(&outcome), vec!["bash.env"]);
    assert_eq!(
        fixture.ctx.config().bash.env.get("TZ").map(String::as_str),
        Some("Etc/GMT")
    );
    assert_eq!(
        fixture
            .ctx
            .config()
            .bash
            .env
            .get("LANG")
            .map(String::as_str),
        Some("C.UTF-8")
    );
}

#[test]
fn project_bash_env_and_runon_env_update_live() {
    // A project's `bash.env` lives in `project_env` and a user's
    // `bash.runon_env` in `runon_env`; editing either must take effect on the
    // next command, and deleting it must clear the old value.
    let fixture = Fixture::new(
        r#"{ "bash": { "env": { "TZ": "UTC" }, "runon_env": ["TZ"] } }"#,
        Some(r#"{ "bash": { "env": { "IBToolNeverDeque": "1" } } }"#),
    );
    assert_eq!(
        fixture
            .ctx
            .config()
            .bash
            .project_env
            .get("IBToolNeverDeque")
            .map(String::as_str),
        Some("1")
    );

    write(
        &fixture.project_path,
        r#"{ "bash": { "env": { "IBToolNeverDeque": "0", "SECOND": "2" } } }"#,
    );
    write(
        &fixture.user_path,
        r#"{ "bash": { "env": { "TZ": "UTC" } } }"#,
    );
    let outcome = fixture.reload();
    assert!(
        applied(&outcome).contains(&"bash.project_env"),
        "{:?}",
        applied(&outcome)
    );
    assert_eq!(
        fixture
            .ctx
            .config()
            .bash
            .project_env
            .get("IBToolNeverDeque")
            .map(String::as_str),
        Some("0"),
        "an edited project value must apply live"
    );
    assert!(fixture.ctx.config().bash.runon_env.is_empty());
    assert!(
        applied(&outcome).contains(&"bash.runon_env"),
        "{:?}",
        applied(&outcome)
    );

    write(&fixture.project_path, "{}");
    let outcome = fixture.reload();
    assert!(
        fixture.ctx.config().bash.project_env.is_empty(),
        "a deleted project map must not keep its old values: {:?}",
        fixture.ctx.config().bash.project_env
    );
    assert!(
        applied(&outcome)
            .iter()
            .any(|key| *key == "bash.project_env"),
        "{:?}",
        applied(&outcome)
    );
}

#[test]
fn user_file_edit_applies_live_too() {
    let fixture = Fixture::new(r#"{ "url_fetch_allow_private": false }"#, None);
    write(&fixture.user_path, r#"{ "url_fetch_allow_private": true }"#);
    let outcome = fixture.reload();
    assert_eq!(applied(&outcome), vec!["url_fetch_allow_private"]);
    assert!(fixture.ctx.config().url_fetch_allow_private);
}

#[test]
fn deferred_keys_are_listed_and_not_applied_until_the_next_configure() {
    let fixture = Fixture::new("{}", Some("{}"));
    let generation = fixture.ctx.configure_generation();
    assert!(fixture.ctx.config().indexes.trigram);
    assert!(!fixture
        .ctx
        .config()
        .disabled_tools
        .iter()
        .any(|tool| tool == "aft_zoom"));

    write(
        &fixture.project_path,
        r#"{ "disabled_tools": ["aft_zoom"], "indexes": { "trigram": false }, "validate_on_edit": "syntax" }"#,
    );
    let outcome = fixture.reload();

    assert_eq!(applied(&outcome), vec!["validate_on_edit"]);
    assert_eq!(
        deferred(&outcome),
        vec!["disabled_tools", "indexes.trigram"]
    );
    let config = fixture.ctx.config();
    assert_eq!(config.validate_on_edit.as_deref(), Some("syntax"));
    assert!(config.indexes.trigram, "a B key must not change live");
    assert!(
        !config.disabled_tools.iter().any(|tool| tool == "aft_zoom"),
        "a C key must not change live"
    );
    assert_eq!(fixture.ctx.configure_generation(), generation);

    // Deferred keys stay listed while they wait for a connect.
    write(
        &fixture.project_path,
        r#"{ "disabled_tools": ["aft_zoom"], "indexes": { "trigram": false }, "validate_on_edit": "full" }"#,
    );
    assert_eq!(
        deferred(&fixture.reload()),
        vec!["disabled_tools", "indexes.trigram"]
    );

    // The next configure applies them as before.
    fixture.configure();
    let config = fixture.ctx.config();
    assert!(!config.indexes.trigram);
    assert!(config.disabled_tools.iter().any(|tool| tool == "aft_zoom"));
}

#[test]
fn invalid_edits_keep_the_last_good_config() {
    let fixture = Fixture::new("{}", Some(r#"{ "format_on_edit": true }"#));
    write(
        &fixture.project_path,
        r#"{ "format_on_edit": true, "bash": { "enabled": false } }"#,
    );
    assert_eq!(applied(&fixture.reload()), vec!["bash.enabled"]);

    for (label, text) in [
        ("truncated JSONC", r#"{ "bash": { "enabled": true "#),
        ("not an object", "[1, 2]"),
        (
            "one bad value next to good ones",
            r#"{ "bash": { "enabled": "nope" }, "format_on_edit": false }"#,
        ),
    ] {
        write(&fixture.project_path, text);
        let outcome = fixture.reload();
        assert!(
            matches!(outcome, ReloadOutcome::Kept { .. }),
            "{label}: expected the last good config to stay, got {outcome:?}"
        );
        let config = fixture.ctx.config();
        assert!(
            !config.bash.enabled,
            "{label}: bash.enabled fell back to its default"
        );
        assert!(config.format_on_edit, "{label}: format_on_edit changed");
    }

    // Fixing the file applies it.
    write(&fixture.project_path, r#"{ "format_on_edit": false }"#);
    let outcome = fixture.reload();
    assert_eq!(applied(&outcome), vec!["format_on_edit", "bash.enabled"]);
}

#[test]
fn deleted_config_file_keeps_the_last_good_config() {
    let fixture = Fixture::new(r#"{ "restrict_to_project_root": true }"#, None);
    assert!(fixture.ctx.config().restrict_to_project_root);

    std::fs::remove_file(&fixture.user_path).expect("delete user config");
    let outcome = fixture.reload();

    match outcome {
        ReloadOutcome::Kept { reason } => assert!(reason.contains("was deleted"), "{reason}"),
        other => panic!("expected the last good config to stay, got {other:?}"),
    }
    assert!(fixture.ctx.config().restrict_to_project_root);
    assert!(fixture
        .ctx
        .cache_unavailable_reason()
        .is_some_and(|reason| reason.contains("cache_dir_config_unknown")));
}

#[test]
fn invalid_user_config_live_disables_caches_without_changing_security_floor() {
    for unreadable in [false, true] {
        let fixture = Fixture::new(r#"{"restrict_to_project_root":true}"#, None);
        assert!(fixture.ctx.cache_unavailable_reason().is_none());
        if unreadable {
            std::fs::remove_file(&fixture.user_path).unwrap();
            std::fs::create_dir(&fixture.user_path).unwrap();
        } else {
            write(&fixture.user_path, "{");
        }
        assert!(matches!(fixture.reload(), ReloadOutcome::Kept { .. }));
        assert!(fixture.ctx.config().restrict_to_project_root);
        assert!(fixture
            .ctx
            .cache_unavailable_reason()
            .is_some_and(|reason| reason.contains("cache_dir_config_unknown")));
        if unreadable {
            std::fs::remove_dir(&fixture.user_path).unwrap();
        }
        write(&fixture.user_path, r#"{"restrict_to_project_root":true}"#);
        assert_eq!(fixture.reload(), ReloadOutcome::Unchanged);
        assert!(fixture.ctx.config().cache_config_unknown.is_none());
        assert!(fixture.ctx.cache_unavailable_reason().is_none());
    }
}

#[test]
fn repaired_user_config_with_new_text_turns_caches_back_on_and_keeps_deferred_keys_deferred() {
    let fixture = Fixture::new(r#"{"restrict_to_project_root":true}"#, None);
    assert!(fixture.ctx.cache_unavailable_reason().is_none());
    write(&fixture.user_path, "{");
    assert!(matches!(fixture.reload(), ReloadOutcome::Kept { .. }));
    assert!(fixture
        .ctx
        .cache_unavailable_reason()
        .is_some_and(|reason| reason.contains("cache_dir_config_unknown")));

    // A valid file whose text differs from the last good text: one deferred
    // key changed, nothing live.
    write(
        &fixture.user_path,
        r#"{"restrict_to_project_root":true,"indexes":{"trigram":false}}"#,
    );
    let outcome = fixture.reload();
    assert_eq!(deferred(&outcome), vec!["indexes.trigram"]);
    assert!(applied(&outcome).is_empty());
    let config = fixture.ctx.config();
    assert!(config.cache_config_unknown.is_none());
    assert!(
        fixture.ctx.cache_unavailable_reason().is_none(),
        "{:?}",
        fixture.ctx.cache_unavailable_reason()
    );
    assert!(
        config.indexes.trigram,
        "a deferred key must not change live"
    );
}

#[cfg(unix)]
#[test]
fn unchanged_symlinked_cache_dir_is_not_reported_deferred_on_reload() {
    // The running config holds the directory a symlinked `storage.cache_dir`
    // resolved to; the file still names the link. A reload that leaves the
    // setting alone must not list it as waiting for the next connect.
    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("cache-target");
    std::fs::create_dir(&target).unwrap();
    let link = temp.path().join("cache-link");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let user = |extra: &str| {
        json!({"storage": {"cache_dir": link}, "format_on_edit": extra == "on"}).to_string()
    };
    let fixture = Fixture::new(&user("off"), None);
    assert_eq!(
        fixture.ctx.config().cache_dir,
        Some(std::fs::canonicalize(&target).unwrap()),
        "the running config holds the resolved directory, not the link"
    );
    write(&fixture.user_path, &user("on"));
    let outcome = fixture.reload();
    assert_eq!(applied(&outcome), vec!["format_on_edit"]);
    assert!(deferred(&outcome).is_empty(), "{outcome:?}");
}

#[test]
fn invalid_unrelated_user_edit_live_keeps_caches_on() {
    let fixture = Fixture::new(r#"{"restrict_to_project_root":true}"#, None);
    write(
        &fixture.user_path,
        r#"{"restrict_to_project_root":true,"semantic":{"backend":"openai-compatible"}}"#,
    );
    // The invalid edit is still rejected as a whole, but it does not hide
    // where caches belong, so caches stay on.
    assert!(matches!(fixture.reload(), ReloadOutcome::Kept { .. }));
    assert!(fixture.ctx.config().restrict_to_project_root);
    assert!(
        fixture.ctx.cache_unavailable_reason().is_none(),
        "{:?}",
        fixture.ctx.cache_unavailable_reason()
    );
}

#[test]
fn deleting_project_config_applies_ordinary_settings_and_holds_security() {
    let project = r#"{ "format_on_edit": true, "sandbox": { "profile": { "head": "strict" } } }"#;
    let fixture = Fixture::new(
        r#"{ "format_on_edit": false, "sandbox": { "profile": { "head": "off" } } }"#,
        Some(project),
    );
    assert!(fixture.ctx.config().format_on_edit);
    assert!(fixture.ctx.config().sandbox.head_enabled());

    std::fs::remove_file(&fixture.project_path).expect("delete project config");
    let outcome = fixture.reload();

    assert!(
        applied(&outcome).contains(&"format_on_edit"),
        "deleting the project file must apply ordinary settings: {outcome:?}"
    );
    assert_eq!(held(&outcome), vec!["sandbox.profile.head"]);
    let config = fixture.ctx.config();
    assert!(!config.format_on_edit);
    assert!(config.sandbox.head_enabled());

    write(&fixture.project_path, project);
    let restored = fixture.reload();
    assert!(held(&restored).is_empty());
    assert!(applied(&restored).contains(&"format_on_edit"));
    assert!(fixture.ctx.config().format_on_edit);
    assert!(fixture.ctx.config().sandbox.head_enabled());
}

#[test]
fn absent_file_that_was_absent_at_connect_is_not_an_error() {
    let fixture = Fixture::new("{}", None);
    write(&fixture.user_path, r#"{ "format_on_edit": true }"#);
    assert_eq!(applied(&fixture.reload()), vec!["format_on_edit"]);
}

#[test]
fn unchanged_file_text_is_a_no_op() {
    let fixture = Fixture::new("{}", Some(r#"{ "format_on_edit": true }"#));
    assert_eq!(fixture.reload(), ReloadOutcome::Unchanged);
}

#[test]
fn project_tier_still_cannot_loosen_on_live_reload() {
    let fixture = Fixture::new(
        r#"{ "restrict_to_project_root": true, "sandbox": { "profile": { "head": "strict" } } }"#,
        Some("{}"),
    );
    assert!(fixture.ctx.config().restrict_to_project_root);
    assert!(fixture.ctx.config().sandbox.head_enabled());

    write(
        &fixture.project_path,
        r#"{ "restrict_to_project_root": false, "sandbox": { "profile": { "head": "off" } }, "format_on_edit": true }"#,
    );
    let outcome = fixture.reload();

    assert_eq!(applied(&outcome), vec!["format_on_edit"]);
    let ReloadOutcome::Reloaded { dropped, .. } = outcome else {
        unreachable!()
    };
    assert!(
        dropped
            .iter()
            .any(|drop| drop.starts_with("restrict_to_project_root")),
        "the dropped project key is reported: {dropped:?}"
    );
    let config = fixture.ctx.config();
    assert!(config.restrict_to_project_root);
    assert!(config.sandbox.head_enabled());
}

#[test]
fn request_pins_its_config_while_a_reload_publishes() {
    let fixture = Fixture::new("{}", Some("{}"));
    assert!(!fixture.ctx.config().restrict_to_project_root);
    write(
        &fixture.user_path,
        r#"{ "restrict_to_project_root": true, "sandbox": { "profile": { "head": "strict" } } }"#,
    );

    let principal = crate::sandbox_spawn::AuthenticatedPrincipal::FirstParty;
    let _pin = fixture.ctx.pin_config();
    // Maintenance can publish a reload on a different thread from the tool call.
    std::thread::scope(|scope| {
        scope.spawn(|| {
            assert_eq!(
                applied(&fixture.reload()),
                vec!["restrict_to_project_root", "sandbox.profile.head"]
            );
        });
    });

    // This request was admitted before the publication: it keeps the old
    // values for every check it still makes.
    assert!(!fixture.ctx.config().restrict_to_project_root);
    assert!(!crate::sandbox_spawn::native_sandbox_enforced(
        &fixture.ctx,
        &principal
    ));
    // A request admitted afterwards sees the new values.
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let _pin = fixture.ctx.pin_config();
            assert!(fixture.ctx.config().restrict_to_project_root);
            assert_eq!(
                crate::sandbox_spawn::native_sandbox_enforced(&fixture.ctx, &principal),
                cfg!(unix)
            );
        });
    });
    assert!(fixture.ctx.config_unpinned().restrict_to_project_root);
}

#[test]
fn configure_publication_updates_its_own_pin_but_a_setter_does_not() {
    let fixture = Fixture::new("{}", None);
    let _pin = fixture.ctx.pin_config();
    // A setter publishing from inside a request leaves the request on its
    // admitted snapshot.
    fixture
        .ctx
        .update_config(|config| config.format_on_edit = true);
    assert!(!fixture.ctx.config().format_on_edit);
    assert!(fixture.ctx.config_unpinned().format_on_edit);
    // Configure (set_config) sees its own publication.
    let mut next = fixture.ctx.config_unpinned().as_ref().clone();
    next.validate_on_edit = Some("syntax".to_string());
    fixture.ctx.set_config(next);
    assert_eq!(
        fixture.ctx.config().validate_on_edit.as_deref(),
        Some("syntax")
    );
}

#[test]
fn a_setter_racing_a_reload_does_not_undo_the_reload() {
    let fixture = Fixture::new("{}", None);
    write(
        &fixture.user_path,
        r#"{ "restrict_to_project_root": true }"#,
    );
    let compress_before = fixture.ctx.config_unpinned().experimental_bash_compress;
    std::thread::scope(|scope| {
        let mut reload_thread = None;
        fixture.ctx.update_config(|config| {
            // The reload publishes while this setter is between its read and
            // its write.
            reload_thread = Some(scope.spawn(|| fixture.reload()));
            std::thread::sleep(Duration::from_millis(300));
            config.experimental_bash_compress = !config.experimental_bash_compress;
        });
        let outcome = reload_thread.unwrap().join().unwrap();
        assert_eq!(applied(&outcome), vec!["restrict_to_project_root"]);
    });
    let config = fixture.ctx.config();
    assert!(
        config.restrict_to_project_root,
        "the setter overwrote the reload"
    );
    assert_ne!(config.experimental_bash_compress, compress_before);
}

#[test]
fn reload_does_not_overwrite_a_configure_that_published_meanwhile() {
    let fixture = Fixture::new("{}", Some("{}"));
    let stale = fixture.ctx.config_unpinned();
    fixture.ctx.update_config(|config| {
        config.checker.insert("rust".into(), "cargo".into());
    });
    let mut next = stale.as_ref().clone();
    next.format_on_edit = true;
    assert!(!fixture.ctx.publish_config_if_current(&stale, next));
    assert!(!fixture.ctx.config().format_on_edit);
    assert_eq!(
        fixture.ctx.config().checker.get("rust").map(String::as_str),
        Some("cargo")
    );
}

#[test]
fn root_without_a_project_watcher_watches_its_config_files_itself() {
    enable_config_watches_for_test();
    let attach_gate = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
    let fixture = with_config_watch_attach_gate(attach_gate.clone(), || {
        Fixture::new("{}", Some(r#"{ "bash": { "enabled": true } }"#))
    });
    // Unit tests start no project watcher, so the root's own watches cover
    // both files.
    assert!(fixture.ctx.config_live().has_project_fallback_watch());
    assert!(fixture.ctx.config_live().has_user_watch());
    // Watch handles exist before their threads attach. Hold that startup until
    // the fixture is configured, then await backend registration before editing.
    *attach_gate.0.lock().unwrap() = true;
    attach_gate.1.notify_all();
    fixture.ctx.config_live().wait_for_watch_attachment();

    write(&fixture.project_path, r#"{ "bash": { "enabled": false } }"#);
    let deadline = Instant::now() + Duration::from_secs(10);
    let outcome = loop {
        if let Some(outcome) = drain_config_reload(&fixture.ctx) {
            break outcome;
        }
        assert!(
            Instant::now() < deadline,
            "no reload was requested by the watch"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(applied(&outcome), vec!["bash.enabled"]);
    assert!(!fixture.ctx.config().bash.enabled);
}

#[test]
fn invalid_active_harness_block_keeps_the_last_good_config() {
    let fixture = Fixture::new(
        r#"{ "restrict_to_project_root": false, "harnesses": { "opencode": { "restrict_to_project_root": true } } }"#,
        None,
    );
    assert!(fixture.ctx.config().restrict_to_project_root);

    write(
        &fixture.user_path,
        r#"{ "restrict_to_project_root": false, "format_on_edit": true, "harnesses": { "opencode": { "restrict_to_project_root": true, "format_on_edit": "yes" } } }"#,
    );
    let outcome = fixture.reload();

    assert!(
        matches!(outcome, ReloadOutcome::Kept { .. }),
        "expected the last good config to stay, got {outcome:?}"
    );
    assert!(fixture.ctx.config().restrict_to_project_root);

    // A block for another harness is not applied here and is not checked.
    write(
        &fixture.user_path,
        r#"{ "restrict_to_project_root": false, "format_on_edit": true, "harnesses": { "opencode": { "restrict_to_project_root": true }, "pi": { "format_on_edit": "yes" } } }"#,
    );
    assert_eq!(applied(&fixture.reload()), vec!["format_on_edit"]);
    assert!(fixture.ctx.config().restrict_to_project_root);
}

fn held(outcome: &ReloadOutcome) -> Vec<&'static str> {
    match outcome {
        ReloadOutcome::Reloaded { held, .. } => held.clone(),
        other => panic!("expected a reload, got {other:?}"),
    }
}

#[test]
fn project_edit_cannot_remove_project_hardening_until_the_next_connect() {
    let fixture = Fixture::new(
        r#"{ "sandbox": { "profile": { "head": "strict" } } }"#,
        Some(r#"{ "sandbox": { "read_deny": ["/secrets"] } }"#),
    );
    let secrets = PathBuf::from("/secrets");
    assert!(fixture.ctx.config().sandbox.read_deny.contains(&secrets));

    write(&fixture.project_path, r#"{ "format_on_edit": true }"#);
    let outcome = fixture.reload();
    assert_eq!(applied(&outcome), vec!["format_on_edit"]);
    assert_eq!(held(&outcome), vec!["sandbox.read_deny"]);
    assert!(fixture.ctx.config().sandbox.read_deny.contains(&secrets));

    // A later user-file edit keeps holding it.
    write(
        &fixture.user_path,
        r#"{ "sandbox": { "profile": { "head": "strict" } }, "validate_on_edit": "syntax" }"#,
    );
    let outcome = fixture.reload();
    assert_eq!(applied(&outcome), vec!["validate_on_edit"]);
    assert!(fixture.ctx.config().sandbox.read_deny.contains(&secrets));

    // A connect resolves the files afresh and applies the loosening.
    fixture.configure();
    assert!(!fixture.ctx.config().sandbox.read_deny.contains(&secrets));
}

#[test]
fn project_edit_cannot_turn_off_a_sandbox_the_project_turned_on() {
    let fixture = Fixture::new(
        "{}",
        Some(r#"{ "sandbox": { "profile": { "head": "strict" } } }"#),
    );
    assert!(fixture.ctx.config().sandbox.head_enabled());

    write(&fixture.project_path, "{}");
    let outcome = fixture.reload();

    assert_eq!(held(&outcome), vec!["sandbox.profile.head"]);
    assert!(fixture.ctx.config().sandbox.head_enabled());
}

#[test]
fn privacy_disclaim_is_live_and_project_removal_cannot_loosen_it() {
    let fixture = Fixture::new("{}", Some("{}"));
    write(
        &fixture.project_path,
        r#"{"bash":{"disclaim_privacy":true}}"#,
    );
    assert_eq!(applied(&fixture.reload()), vec!["bash.disclaim_privacy"]);
    assert!(fixture.ctx.config().bash.disclaim_privacy);
    write(&fixture.project_path, "{}");
    assert_eq!(held(&fixture.reload()), vec!["bash.disclaim_privacy"]);
    assert!(fixture.ctx.config().bash.disclaim_privacy);
}

#[test]
fn user_can_live_reload_privacy_disclaim_off() {
    let fixture = Fixture::new(r#"{"bash":{"disclaim_privacy":true}}"#, Some("{}"));
    write(&fixture.user_path, r#"{"bash":{"disclaim_privacy":false}}"#);
    assert_eq!(applied(&fixture.reload()), vec!["bash.disclaim_privacy"]);
    assert!(!fixture.ctx.config().bash.disclaim_privacy);
}

#[test]
fn a_user_edit_can_still_loosen_what_the_user_set() {
    let fixture = Fixture::new(
        r#"{ "sandbox": { "profile": { "head": "strict" } } }"#,
        Some("{}"),
    );
    write(
        &fixture.user_path,
        r#"{ "sandbox": { "profile": { "head": "off" } } }"#,
    );
    let outcome = fixture.reload();
    assert_eq!(applied(&outcome), vec!["sandbox.profile.head"]);
    assert!(held(&outcome).is_empty());
    assert!(!fixture.ctx.config().sandbox.head_enabled());
}

#[test]
fn deleting_a_file_backed_relayed_tier_holds_security_until_recreated() {
    let temp = tempfile::tempdir().expect("temp dir");
    let root = temp.path().join("project");
    std::fs::create_dir_all(&root).unwrap();
    let ctx = crate::test_context(
        Box::new(TreeSitterProvider::new()),
        Config {
            storage_dir: Some(temp.path().join("storage")),
            ..Config::default()
        },
    );
    let relayed = r#"{ "sandbox": { "profile": { "head": "strict" } } }"#;
    let req: RawRequest = serde_json::from_value(json!({
        "id": "configure-wire",
        "command": "configure",
        "project_root": root,
        "harness": "opencode",
        // An absent user file of its own, so a user config path another test
        // registered for the process is not read here.
        "cortexkit_user_config_path": temp.path().join("xdg/cortexkit/aft.jsonc"),
        "config": [{ "tier": "project", "source": "wire", "doc": relayed }],
    }))
    .unwrap();
    let _git_env = crate::test_env::hermetic_git_env_guard();
    assert!(crate::commands::configure::handle_configure(&req, &ctx).success);
    let project_path = project_config_path(&root);
    write(&project_path, relayed);
    assert_eq!(reload_config_now(&ctx), ReloadOutcome::Unchanged);
    assert!(ctx.config().sandbox.head_enabled());

    std::fs::remove_file(&project_path).unwrap();
    let deleted = reload_config_now(&ctx);
    assert_eq!(held(&deleted), vec!["sandbox.profile.head"]);
    assert!(ctx.config().sandbox.head_enabled());

    write(&project_path, relayed);
    let restored = reload_config_now(&ctx);
    assert!(held(&restored).is_empty());
    assert!(ctx.config().sandbox.head_enabled());
}

#[test]
fn successive_held_project_edits_keep_all_published_project_hardening() {
    let fixture = Fixture::new(
        r#"{ "sandbox": { "profile": { "head": "strict" }, "read_deny": ["/u"] } }"#,
        Some(r#"{ "sandbox": { "read_deny": ["/a"] } }"#),
    );
    let deny = |path: &str| {
        fixture
            .ctx
            .config()
            .sandbox
            .read_deny
            .contains(&PathBuf::from(path))
    };
    assert!(deny("/u") && deny("/a"));

    // [A] -> [B]: A is held, B is added.
    write(
        &fixture.project_path,
        r#"{ "sandbox": { "read_deny": ["/b"] } }"#,
    );
    assert_eq!(held(&fixture.reload()), vec!["sandbox.read_deny"]);
    assert!(deny("/u") && deny("/a") && deny("/b"));

    // [B] -> []: B came from a project text that was never recorded, and must
    // stay as well as A.
    write(&fixture.project_path, "{}");
    assert_eq!(held(&fixture.reload()), vec!["sandbox.read_deny"]);
    assert!(
        deny("/u") && deny("/a") && deny("/b"),
        "{:?}",
        fixture.ctx.config().sandbox.read_deny
    );

    // A connect resolves afresh.
    fixture.configure();
    assert!(deny("/u") && !deny("/a") && !deny("/b"));
}

#[test]
fn successive_held_project_edits_keep_a_project_enabled_sandbox() {
    // The project turns the sandbox on and also adds a deny; the next edit
    // drops the deny (held) while keeping the sandbox on, and the one after
    // turns the sandbox off.
    let fixture = Fixture::new("{}", Some(r#"{ "sandbox": { "read_deny": ["/a"] } }"#));
    assert!(!fixture.ctx.config().sandbox.head_enabled());

    write(
        &fixture.project_path,
        r#"{ "sandbox": { "profile": { "head": "strict" } } }"#,
    );
    let outcome = fixture.reload();
    assert_eq!(held(&outcome), vec!["sandbox.read_deny"]);
    assert!(fixture.ctx.config().sandbox.head_enabled());

    write(&fixture.project_path, "{}");
    fixture.reload();
    assert!(
        fixture.ctx.config().sandbox.head_enabled(),
        "a sandbox a held project edit turned on must stay on"
    );
}

#[test]
fn a_replaced_config_directory_is_watched_again() {
    enable_config_watches_for_test();
    let fixture = Fixture::new("{}", Some("{}"));
    assert!(fixture.ctx.config_live().has_project_fallback_watch());
    std::thread::sleep(Duration::from_millis(500));

    // Rename the directory aside and put a new one in its place.
    let config_dir = fixture.project_path.parent().unwrap().to_path_buf();
    let aside = config_dir.with_file_name(".cortexkit-old");
    std::fs::rename(&config_dir, &aside).unwrap();
    write(&fixture.project_path, "{}");
    std::thread::sleep(Duration::from_millis(800));
    let _ = drain_config_reload(&fixture.ctx);

    // A security tightening in the replacement must be applied.
    write(
        &fixture.project_path,
        r#"{ "sandbox": { "profile": { "head": "strict" } } }"#,
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while !fixture.ctx.config().sandbox.head_enabled() {
        assert!(
            Instant::now() < deadline,
            "an edit in the replaced directory was never applied"
        );
        let _ = drain_config_reload(&fixture.ctx);
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn reverting_the_project_file_releases_the_hold() {
    let fixture = Fixture::new(
        r#"{ "sandbox": { "profile": { "head": "strict" }, "read_deny": ["/u"] } }"#,
        Some(r#"{ "sandbox": { "read_deny": ["/a"] } }"#),
    );
    let deny = |path: &str| {
        fixture
            .ctx
            .config()
            .sandbox
            .read_deny
            .contains(&PathBuf::from(path))
    };

    // [A] -> [B]: A is held and B is added.
    write(
        &fixture.project_path,
        r#"{ "sandbox": { "read_deny": ["/b"] } }"#,
    );
    assert_eq!(held(&fixture.reload()), vec!["sandbox.read_deny"]);
    assert!(deny("/a") && deny("/b"));

    // Back to [A]: the held edit is gone, so the published config is the
    // files' again and B, which only the abandoned edit added, goes.
    write(
        &fixture.project_path,
        r#"{ "sandbox": { "read_deny": ["/a"] } }"#,
    );
    let outcome = fixture.reload();
    assert!(held(&outcome).is_empty(), "{outcome:?}");
    assert!(deny("/u") && deny("/a") && !deny("/b"));

    // With the hold released, a user-file loosening applies live again.
    write(
        &fixture.user_path,
        r#"{ "sandbox": { "profile": { "head": "off" }, "read_deny": ["/u"] } }"#,
    );
    let outcome = fixture.reload();
    assert_eq!(applied(&outcome), vec!["sandbox.profile.head"]);
    assert!(!fixture.ctx.config().sandbox.head_enabled());
}

#[test]
fn legacy_project_config_events_trigger_reload_with_an_existing_project_watcher() {
    let _env = crate::test_env::process_env_lock();
    enable_config_watches_for_test();
    for directory in [".opencode", ".pi"] {
        for filename in ["aft.jsonc", "aft.json"] {
            let fixture = Fixture::new(r#"{"format_on_edit":true}"#, Some("{}"));
            let legacy = fixture.root.join(directory).join(filename);
            write(&legacy, r#"{"format_on_edit":false}"#);
            std::fs::remove_file(&fixture.project_path).unwrap();
            fixture.configure();
            fixture.ctx.stop_watcher_runtime();
            // Install an idle project-watcher thread and mark .cortexkit covered.
            // It delivers no events, so the real legacy-file watch must request the reload.
            let (dispatch_tx, dispatch_rx) = crate::watcher_filter::watcher_dispatch_channel();
            let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let thread_shutdown = Arc::clone(&shutdown);
            let join = std::thread::spawn(move || {
                while !thread_shutdown.load(std::sync::atomic::Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(10));
                }
                drop(dispatch_tx);
            });
            fixture.ctx.install_watcher_runtime(
                dispatch_rx,
                crate::watcher_filter::WatcherThreadHandle::new(shutdown, join),
            );
            let state = fixture.ctx.config_live();
            state.set_project_watcher_sees_config(true);
            sync_config_watches(&fixture.ctx);
            assert!(fixture.ctx.watcher_runtime_active());
            assert!(state.project_watcher_sees_config_for_test());
            assert_eq!(
                state.project_fallback_file_for_test().as_deref(),
                Some(legacy.as_path())
            );
            assert!(!fixture.ctx.config().format_on_edit);
            // Drain setup events before observing the next write's reload request.
            std::thread::sleep(Duration::from_millis(500));
            state
                .signal()
                .due_at_ms
                .store(0, std::sync::atomic::Ordering::Release);
            write(&legacy, r#"{"format_on_edit":true}"#);
            let deadline = Instant::now() + Duration::from_secs(5);
            while !state.signal().is_pending() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            assert!(
                state.signal().is_pending(),
                "legacy edit did not request a reload: {}",
                legacy.display()
            );
            assert!(applied(&fixture.reload()).contains(&"format_on_edit"));
            assert!(fixture.ctx.config().format_on_edit);
            assert_eq!(
                std::fs::read_to_string(&legacy).unwrap(),
                r#"{"format_on_edit":true}"#
            );
            assert!(!fixture.project_path.exists());
            fixture.ctx.stop_watcher_runtime();
        }
    }
}
