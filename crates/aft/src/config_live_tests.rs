use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;

use super::*;
use crate::config::Config;
use crate::context::AppContext;
use crate::parser::TreeSitterProvider;
use crate::protocol::RawRequest;

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
        let ctx = AppContext::new(
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

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("config dir");
    std::fs::write(path, text).expect("write config");
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
fn reload_log_line_names_applied_and_deferred_keys() {
    let line = reload_log_line(
        "/p",
        &["bash.enabled"],
        &["disabled_tools"],
        &["restrict_to_project_root (user-only)".to_string()],
    );
    assert_eq!(
        line,
        "config reload root=/p applied=[bash.enabled] deferred=[disabled_tools] \
         (deferred keys apply on next connect/restart) \
         dropped=[restrict_to_project_root (user-only)] (project may only tighten)"
    );
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
        ("retired key", r#"{ "gh_read": true }"#),
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
        r#"{ "restrict_to_project_root": true, "sandbox": { "enabled": true } }"#,
        Some("{}"),
    );
    assert!(fixture.ctx.config().restrict_to_project_root);
    assert!(fixture.ctx.config().sandbox.enabled);

    write(
        &fixture.project_path,
        r#"{ "restrict_to_project_root": false, "sandbox": { "enabled": false }, "format_on_edit": true }"#,
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
    assert!(config.sandbox.enabled);
}

#[test]
fn request_pins_its_config_while_a_reload_publishes() {
    let fixture = Fixture::new("{}", Some("{}"));
    assert!(!fixture.ctx.config().restrict_to_project_root);
    write(
        &fixture.user_path,
        r#"{ "restrict_to_project_root": true, "sandbox": { "enabled": true } }"#,
    );

    let principal = crate::sandbox_spawn::AuthenticatedPrincipal::FirstParty;
    let _pin = fixture.ctx.pin_config();
    // The reload runs on another thread, like the maintenance lane.
    std::thread::scope(|scope| {
        scope.spawn(|| {
            assert_eq!(
                applied(&fixture.reload()),
                vec!["restrict_to_project_root", "sandbox.enabled"]
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
fn a_user_file_edit_asks_every_live_root_to_reload() {
    let first = Arc::new(AppContext::new(
        Box::new(TreeSitterProvider::new()),
        Config::default(),
    ));
    let second = Arc::new(AppContext::new(
        Box::new(TreeSitterProvider::new()),
        Config::default(),
    ));
    request_reload_for_roots(&[Arc::clone(&first), Arc::clone(&second)]);
    assert!(first.config_live().signal().is_pending());
    assert!(second.config_live().signal().is_pending());
}

#[test]
fn signal_debounces_until_quiet() {
    let signal = ConfigReloadSignal::default();
    assert!(!signal.take_if_due());
    signal.request();
    assert!(signal.is_pending());
    assert!(!signal.take_if_due(), "not due inside the debounce window");
    std::thread::sleep(CONFIG_RELOAD_DEBOUNCE + Duration::from_millis(50));
    assert!(signal.take_if_due());
    assert!(!signal.is_pending());
}

#[test]
fn config_file_watch_sees_edits_and_a_directory_created_later() {
    let temp = tempfile::tempdir().expect("temp dir");
    let root = std::fs::canonicalize(temp.path()).expect("canonical temp");
    let file = project_config_path(&root);
    let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = Arc::clone(&hits);
    let _watch = ConfigFileWatch::start_with(
        file.clone(),
        Arc::new(move || {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }),
    );
    // Each wait compares against a count taken BEFORE the write: a watch that
    // fires between the write and a count taken afterwards would otherwise be
    // folded into the baseline and read as a miss.
    let wait_for_hit_after = |before: usize, label: &str| {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if hits.load(std::sync::atomic::Ordering::SeqCst) > before {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("config watch missed {label}");
    };
    // Let the watch attach to the root before `.cortexkit` exists.
    std::thread::sleep(Duration::from_millis(500));
    let before = hits.load(std::sync::atomic::Ordering::SeqCst);
    write(&file, "{}");
    wait_for_hit_after(before, "the directory and file being created");
    std::thread::sleep(Duration::from_millis(500));
    let before = hits.load(std::sync::atomic::Ordering::SeqCst);
    write(&file, r#"{ "format_on_edit": true }"#);
    wait_for_hit_after(before, "an edit");
}

#[test]
fn project_watcher_filter_recognises_the_config_file() {
    let root = Path::new("/r");
    assert!(is_project_config_event_path(
        root,
        Path::new("/r/.cortexkit/aft.jsonc")
    ));
    assert!(is_project_config_event_path(
        root,
        Path::new("/r/.cortexkit/aft.jsonc.tmp.123")
    ));
    assert!(!is_project_config_event_path(
        root,
        Path::new("/r/.cortexkit/other.json")
    ));
    assert!(!is_project_config_event_path(
        root,
        Path::new("/r/aft.jsonc")
    ));
}

#[test]
fn root_without_a_project_watcher_watches_its_config_files_itself() {
    enable_config_watches_for_test();
    let fixture = Fixture::new("{}", Some(r#"{ "bash": { "enabled": true } }"#));
    // Unit tests start no project watcher, so the root's own watches cover
    // both files.
    assert!(fixture.ctx.config_live().has_project_fallback_watch());
    assert!(fixture.ctx.config_live().has_user_watch());
    std::thread::sleep(Duration::from_millis(500));

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
fn project_watcher_signals_a_config_edit_under_a_target_ancestor() {
    use std::sync::atomic::{AtomicBool, AtomicU64};
    let temp = tempfile::tempdir().expect("temp dir");
    // A root whose own path passes through `target`, which the corpus filters
    // treat as build output.
    let root = std::fs::canonicalize(temp.path())
        .expect("canonical temp")
        .join("target")
        .join("repo");
    let config_file = project_config_path(&root);
    write(&config_file, "{}");
    let signal = Arc::new(ConfigReloadSignal::default());
    let shutdown = Arc::new(AtomicBool::new(false));
    let (dispatch_tx, _dispatch_rx) = crossbeam_channel::bounded(8);
    let (raw_sender_tx, raw_sender_rx) = crossbeam_channel::bounded(1);
    let filter_config = crate::watcher_filter::WatcherFilterConfig::new(root.clone(), None)
        .with_config_reload_signal(Arc::clone(&signal));
    let filter_shutdown = Arc::clone(&shutdown);
    let filter = std::thread::spawn(move || {
        crate::watcher_filter::run_watcher_thread(
            filter_config,
            Vec::new(),
            Arc::new(std::sync::RwLock::new(None)),
            Arc::new(AtomicU64::new(0)),
            dispatch_tx,
            filter_shutdown,
            move |_root, _extra, raw_tx| {
                raw_sender_tx.send(raw_tx).unwrap();
                Ok::<(), std::io::Error>(())
            },
        );
    });
    let raw_tx: std::sync::mpsc::Sender<notify::Result<notify::Event>> =
        raw_sender_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    raw_tx
        .send(Ok(notify::Event::new(notify::EventKind::Modify(
            notify::event::ModifyKind::Any,
        ))
        .add_path(config_file)))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while !signal.is_pending() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
    drop(raw_tx);
    filter.join().unwrap();
    assert!(signal.is_pending(), "the config edit never raised a reload");
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
        r#"{ "sandbox": { "enabled": true } }"#,
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
        r#"{ "sandbox": { "enabled": true }, "validate_on_edit": "syntax" }"#,
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
    let fixture = Fixture::new("{}", Some(r#"{ "sandbox": { "enabled": true } }"#));
    assert!(fixture.ctx.config().sandbox.enabled);

    write(&fixture.project_path, "{}");
    let outcome = fixture.reload();

    assert_eq!(held(&outcome), vec!["sandbox.enabled"]);
    assert!(fixture.ctx.config().sandbox.enabled);
}

#[test]
fn a_user_edit_can_still_loosen_what_the_user_set() {
    let fixture = Fixture::new(r#"{ "sandbox": { "enabled": true } }"#, Some("{}"));
    write(&fixture.user_path, r#"{ "sandbox": { "enabled": false } }"#);
    let outcome = fixture.reload();
    assert_eq!(applied(&outcome), vec!["sandbox.enabled"]);
    assert!(held(&outcome).is_empty());
    assert!(!fixture.ctx.config().sandbox.enabled);
}

#[test]
fn a_tier_file_that_appears_with_the_relayed_text_is_then_file_backed() {
    let temp = tempfile::tempdir().expect("temp dir");
    let root = temp.path().join("project");
    std::fs::create_dir_all(&root).unwrap();
    let ctx = AppContext::new(
        Box::new(TreeSitterProvider::new()),
        Config {
            storage_dir: Some(temp.path().join("storage")),
            ..Config::default()
        },
    );
    let relayed = r#"{ "sandbox": { "enabled": true } }"#;
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

    std::fs::remove_file(&project_path).unwrap();
    match Some(reload_config_now(&ctx)) {
        Some(ReloadOutcome::Kept { reason }) => assert!(reason.contains("was deleted"), "{reason}"),
        other => panic!("expected the deletion to be refused, got {other:?}"),
    }
}

#[test]
fn successive_held_project_edits_keep_all_published_project_hardening() {
    let fixture = Fixture::new(
        r#"{ "sandbox": { "enabled": true, "read_deny": ["/u"] } }"#,
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
    assert!(!fixture.ctx.config().sandbox.enabled);

    write(
        &fixture.project_path,
        r#"{ "sandbox": { "enabled": true } }"#,
    );
    let outcome = fixture.reload();
    assert_eq!(held(&outcome), vec!["sandbox.read_deny"]);
    assert!(fixture.ctx.config().sandbox.enabled);

    write(&fixture.project_path, "{}");
    fixture.reload();
    assert!(
        fixture.ctx.config().sandbox.enabled,
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
        r#"{ "sandbox": { "enabled": true } }"#,
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while !fixture.ctx.config().sandbox.enabled {
        assert!(
            Instant::now() < deadline,
            "an edit in the replaced directory was never applied"
        );
        let _ = drain_config_reload(&fixture.ctx);
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_directory_replaced_under_the_same_name_is_attached_again() {
    let temp = tempfile::tempdir().expect("temp dir");
    let dir = temp.path().join(".cortexkit");
    std::fs::create_dir_all(&dir).unwrap();
    let mut attachment = DirAttachment::new(dir.clone());
    let mut watched = Vec::new();
    let mut op = |op: WatchOp<'_>| {
        if let WatchOp::Watch(path) = op {
            watched.push(path.to_path_buf());
        }
        Ok(())
    };
    assert!(attachment.attach(&mut op), "first attach checks the file");
    assert!(
        !attachment.attach(&mut op),
        "an unchanged directory is left alone"
    );

    // Rename the directory aside and put a new one under the same name.
    std::fs::rename(&dir, temp.path().join(".cortexkit-old")).unwrap();
    std::fs::create_dir_all(&dir).unwrap();

    assert!(
        attachment.attach(&mut op),
        "the replacement must be attached and its file checked"
    );
    drop(op);
    assert_eq!(watched, vec![dir.clone(), dir]);
}

#[test]
fn reading_config_inside_update_config_panics_instead_of_deadlocking() {
    let ctx = Arc::new(AppContext::new(
        Box::new(TreeSitterProvider::new()),
        Config::default(),
    ));
    let (tx, rx) = std::sync::mpsc::channel();
    let worker_ctx = Arc::clone(&ctx);
    std::thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            worker_ctx.update_config(|config| {
                config.format_on_edit = worker_ctx.config().format_on_edit;
            });
        }));
        let message = result.err().and_then(|payload| {
            payload
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
        });
        let _ = tx.send(message);
    });
    let message = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("a config read inside update_config hung instead of panicking")
        .expect("a config read inside update_config did not panic");
    assert!(
        message.contains("config read inside update_config closure would deadlock"),
        "{message}"
    );
    // The lock and the flag are released: config is readable and writable.
    ctx.update_config(|config| config.format_on_edit = true);
    assert!(ctx.config().format_on_edit);
}

#[test]
fn a_forced_reattach_watches_the_same_directory_again() {
    // A directory deleted and recreated can get its inode back; an event
    // naming the directory, or a backend error, forces the re-attach.
    let temp = tempfile::tempdir().expect("temp dir");
    let dir = temp.path().join(".cortexkit");
    std::fs::create_dir_all(&dir).unwrap();
    let mut attachment = DirAttachment::new(dir.clone());
    let mut watched = Vec::new();
    let mut unwatched = Vec::new();
    let mut op = |op: WatchOp<'_>| {
        match op {
            WatchOp::Watch(path) => watched.push(path.to_path_buf()),
            WatchOp::Unwatch(path) => unwatched.push(path.to_path_buf()),
        }
        Ok(())
    };
    assert!(attachment.attach(&mut op));
    assert!(!attachment.attach(&mut op));
    assert_eq!(attachment.watched(), Some(dir.as_path()));

    attachment.force_reattach();
    assert!(
        attachment.attach(&mut op),
        "a forced re-attach must watch again and check the file"
    );
    assert!(!attachment.attach(&mut op), "and only once");
    drop(op);
    assert_eq!(watched, vec![dir.clone(), dir.clone()]);
    assert_eq!(unwatched, vec![dir]);
}

#[cfg(target_os = "macos")]
#[test]
fn watcher_config_user_dropped_rescans_without_creating_streams() {
    use notify::{event::Flag, Event, EventKind, RecursiveMode};
    let temp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap();
    // Fresh worktrees have no .cortexkit directory. The config fallback watches
    // the root until that directory appears, without recursive exclusions.
    let dir = root.join(".cortexkit");
    let counters = crate::context::watcher_counters_for_root(&root);
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut watcher = crate::watcher_backend::DirectoryWatcher::new(tx, Arc::clone(&counters));
    let mut attachment = DirAttachment::new(dir.clone());
    let mut attach = |attachment: &mut DirAttachment| {
        attachment.attach(&mut |op| match op {
            WatchOp::Watch(path) => watcher
                .watch(path, RecursiveMode::NonRecursive)
                .map_err(|e| e.to_string()),
            WatchOp::Unwatch(path) => watcher.unwatch(path).map_err(|e| e.to_string()),
        })
    };
    assert!(!attach(&mut attachment)); // attached to root, not yet to config dir
    let initial = counters.snapshot().fsevents_stream_creations_total;
    assert_eq!(initial, 1, "the counter must reach FSEventStreamCreate");
    let rescans = std::cell::Cell::new(0);
    let rescan = || rescans.set(rescans.get() + 1);
    for _ in 0..8 {
        let event = Event::new(EventKind::Other)
            .set_flag(Flag::Rescan)
            .set_info("rescan: user dropped")
            .add_path(root.clone());
        handle_config_watch_event(&dir, &event, &mut attachment, &rescan);
        attach(&mut attachment);
    }
    assert_eq!(
        counters.snapshot().fsevents_stream_creations_total - initial,
        0,
        "user_dropped must not reattach the config fallback stream"
    );
    assert_eq!(rescans.get(), 8, "every drop must request a content rescan");
}

#[test]
fn publishing_inside_update_config_panics_instead_of_deadlocking() {
    let ctx = Arc::new(AppContext::new(
        Box::new(TreeSitterProvider::new()),
        Config::default(),
    ));
    let (tx, rx) = std::sync::mpsc::channel();
    let worker_ctx = Arc::clone(&ctx);
    std::thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            worker_ctx.update_config(|config| {
                config.format_on_edit = true;
                // A setter that publishes on its own.
                worker_ctx.update_config(|inner| inner.validate_on_edit = None);
            });
        }));
        let message = result.err().and_then(|payload| {
            payload
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
        });
        let _ = tx.send(message);
    });
    let message = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("a nested publication hung instead of panicking")
        .expect("a nested publication did not panic");
    assert!(
        message.contains("config read inside update_config closure would deadlock"),
        "{message}"
    );
    ctx.update_config(|config| config.format_on_edit = true);
    assert!(ctx.config().format_on_edit);
}

#[test]
fn reverting_the_project_file_releases_the_hold() {
    let fixture = Fixture::new(
        r#"{ "sandbox": { "enabled": true, "read_deny": ["/u"] } }"#,
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
        r#"{ "sandbox": { "enabled": false, "read_deny": ["/u"] } }"#,
    );
    let outcome = fixture.reload();
    assert_eq!(applied(&outcome), vec!["sandbox.enabled"]);
    assert!(!fixture.ctx.config().sandbox.enabled);
}
