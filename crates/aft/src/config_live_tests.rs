use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;

use super::*;
use crate::config::Config;
use crate::context::AppContext;
use crate::parser::TreeSitterProvider;

#[test]
fn remote_switch_and_project_refusal_apply_live_but_demand_waits_for_reconnect() {
    let mut published = Config::default();
    published.remote_exec.enabled = true;
    published.remote_exec.default_demand = Some("linux".into());
    let mut candidate = published.clone();
    candidate.remote_exec.enabled = false;
    candidate.remote_exec.project_off = true;
    candidate.remote_exec.default_demand = Some("windows".into());
    let off = apply_live_config(&published, &candidate, &published);
    assert!(!off.next.remote_exec.enabled);
    assert!(off.next.remote_exec.project_off);
    assert_eq!(
        off.next.remote_exec.default_demand.as_deref(),
        Some("linux")
    );
    assert_eq!(
        off.applied,
        ["remote_exec.enabled", "remote_exec.project_off"]
    );
    assert_eq!(off.deferred, ["remote_exec.default_demand"]);
    let on = apply_live_config(&off.next, &published, &published);
    assert!(on.next.remote_exec.enabled);
    assert!(!on.next.remote_exec.project_off);
    assert!(on.deferred.is_empty());
}

#[test]
fn shell_env_provider_changes_apply_to_the_next_call() {
    let published = Config::default();
    let mut candidate = published.clone();
    candidate.shell = serde_json::from_value(json!({"env_provider":{"kind":"mise"}})).unwrap();
    let applied = apply_live_config(&published, &candidate, &published);
    assert_eq!(applied.next.shell, candidate.shell);
    assert!(applied.applied.contains(&"shell.env_provider"));
    let disabled = apply_live_config(&applied.next, &published, &published);
    assert!(disabled.next.shell.env_provider.is_none());
    assert!(disabled.applied.contains(&"shell.env_provider"));
}

pub(crate) fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("config dir");
    std::fs::write(path, text).expect("write config");
}

#[test]
fn bash_env_updates_live_while_cache_dir_waits_for_reconfigure() {
    let published = Config::default();
    let mut candidate = published.clone();
    candidate.bash.env.insert("TZ".into(), "UTC".into());
    candidate.cache_dir = Some("/cache/next".into());
    let result = super::apply_live_config(&published, &candidate, &published);
    assert_eq!(result.applied, vec!["bash.env"]);
    assert_eq!(result.deferred, vec!["storage.cache_dir"]);
    assert_eq!(result.next.bash.env["TZ"], "UTC");
    assert_eq!(result.next.cache_dir, published.cache_dir);
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
        handle_config_watch_event(&dir.join("aft.jsonc"), &event, &mut attachment, &rescan);
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
fn idle_regular_config_checks_symlinks_only_once_per_two_seconds() {
    let dir = tempfile::tempdir().unwrap();
    let file = std::fs::canonicalize(dir.path()).unwrap().join("aft.jsonc");
    std::fs::write(&file, "{}").unwrap();
    let start = Instant::now();
    let mut next_check = start;
    let mut checks = 0;
    for tick in 0..=30 {
        let now = start + CONFIG_WATCH_POLL * tick;
        recheck_config_target(&mut next_check, now, false, |poll_content| {
            checks += 1;
            assert!(!config_path_is_symlink(&file));
            assert!(poll_content);
            false
        });
        assert_eq!(checks, 1 + tick / 10, "symlink checks at idle tick {tick}");
    }
    assert_eq!(checks, 4, "startup plus three two-second revalidations");
}

#[test]
fn config_target_events_refresh_immediately_without_extra_fingerprint_polls() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("settings.jsonc");
    std::fs::write(&file, "{}").unwrap();
    let mut attachment = DirAttachment::new(dir.path().to_path_buf());
    assert!(attachment.attach(&mut |_| Ok(())));
    let event = notify::Event::new(notify::EventKind::Modify(notify::event::ModifyKind::Data(
        notify::event::DataChange::Any,
    )))
    .add_path(file.clone());
    let start = Instant::now();
    let mut next_check = start;
    let mut refreshes = 0;
    let mut polls = 0;
    for tick in 0..=10 {
        let now = start + CONFIG_WATCH_POLL * tick;
        let relevant = handle_config_watch_event(&file, &event, &mut attachment, &|| {});
        assert!(
            relevant,
            "the target's actual filename must trigger discovery"
        );
        recheck_config_target(&mut next_check, now, relevant, |poll_content| {
            refreshes += 1;
            polls += u32::from(poll_content);
            false
        });
    }
    assert_eq!(refreshes, 11, "events bypass the idle discovery deadline");
    assert_eq!(
        polls, 2,
        "fingerprints are polled only at startup and two seconds"
    );
    let unrelated = notify::Event::new(notify::EventKind::Modify(notify::event::ModifyKind::Data(
        notify::event::DataChange::Any,
    )))
    .add_path(dir.path().join("unrelated"));
    assert!(!handle_config_watch_event(
        &file,
        &unrelated,
        &mut attachment,
        &|| {}
    ));
}

#[test]
fn config_target_fallback_hashes_bounded_content_and_stops_when_attached() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("settings.jsonc");
    std::fs::write(&file, "before").unwrap();
    let modified =
        filetime::FileTime::from_last_modification_time(&std::fs::metadata(&file).unwrap());
    let mut target = ConfigTargetWatch {
        file: file.clone(),
        watcher: None,
        attachment: DirAttachment::new(dir.path().to_path_buf()),
        fingerprint: None,
    };
    assert!(target.poll_if_unattached(&file));
    assert!(!target.poll_if_unattached(&file));
    std::fs::write(&file, "after!").unwrap();
    filetime::set_file_mtime(&file, modified).unwrap();
    assert!(
        target.poll_if_unattached(&file),
        "same size and mtime still need a hash"
    );
    std::fs::remove_file(&file).unwrap();
    assert!(target.poll_if_unattached(&file));
    std::fs::write(&file, "return").unwrap();
    assert!(target.poll_if_unattached(&file));

    let bytes = vec![b'a'; CONFIG_READ_LIMIT + 32];
    std::fs::write(&file, &bytes).unwrap();
    let before = config_file_fingerprint(&file).unwrap();
    let mut bytes = bytes;
    bytes[CONFIG_READ_LIMIT] = b'b';
    std::fs::write(&file, bytes).unwrap();
    filetime::set_file_mtime(
        &file,
        filetime::FileTime::from_system_time(before.modified.unwrap()),
    )
    .unwrap();
    assert_eq!(
        config_file_fingerprint(&file).unwrap().hash,
        before.hash,
        "fallback reads no more than the config size limit"
    );

    assert!(target.attachment.attach(&mut |_| Ok(())));
    assert!(!target.poll_if_unattached(&file));
    assert!(
        target.fingerprint.is_none(),
        "an attached target does not retain a polling snapshot"
    );
}

#[cfg(any(unix, windows))]
#[test]
fn config_target_shares_canonical_directory_watch_and_keeps_actual_filename() {
    let dir = tempfile::tempdir().unwrap();
    let config_dir = dir.path().join("config");
    let alias = dir.path().join("config-alias");
    std::fs::create_dir(&config_dir).unwrap();
    let original = config_dir.join("aft.jsonc");
    std::fs::write(&original, "{}").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&config_dir, &alias).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_dir(&config_dir, &alias).unwrap();
    let file = alias.join("aft.jsonc");
    let mut attachment = DirAttachment::new(alias.clone());
    assert!(attachment.attach(&mut |_| Ok(())));
    let (tx, _rx) = mpsc::channel();
    let counters = Arc::new(crate::context::WatcherCounters::default());
    let mut target = None;
    assert!(refresh_config_target_watch(
        &file,
        &mut target,
        &attachment,
        &tx,
        &counters,
        true,
    ));
    {
        let target = target.as_ref().unwrap();
        assert!(
            target.watcher.is_none(),
            "an ancestor alias shares the link's watch"
        );
        assert!(target.attachment.watched().is_none());
        assert!(
            target.fingerprint.is_none(),
            "the shared watch needs no content poll"
        );
    }

    let actual = config_dir.join("settings.jsonc");
    std::fs::write(&actual, r#"{"bash":{"enabled":false}}"#).unwrap();
    std::fs::remove_file(&original).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&actual, &original).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_file(&actual, &original).unwrap();
    assert!(refresh_config_target_watch(
        &file,
        &mut target,
        &attachment,
        &tx,
        &counters,
        true,
    ));
    {
        let target = target.as_mut().unwrap();
        assert!(
            target.watcher.is_none(),
            "a same-folder file link also shares the watch"
        );
        assert_eq!(target.file, std::fs::canonicalize(&actual).unwrap());
        let changes = std::cell::Cell::new(0);
        let event = notify::Event::new(notify::EventKind::Modify(notify::event::ModifyKind::Data(
            notify::event::DataChange::Any,
        )))
        .add_path(target.file.clone());
        assert!(handle_config_watch_event(
            &target.file,
            &event,
            &mut target.attachment,
            &|| changes.set(changes.get() + 1),
        ));
        assert_eq!(
            changes.get(),
            1,
            "the shared watch recognizes the target's actual name"
        );
    }

    let unattached = DirAttachment::new(alias);
    assert!(refresh_config_target_watch(
        &file,
        &mut target,
        &unattached,
        &tx,
        &counters,
        true,
    ));
    assert!(target.as_ref().unwrap().watcher.is_none());
    assert!(
        target.as_ref().unwrap().fingerprint.is_some(),
        "a failed shared attachment still polls"
    );
    refresh_config_target_watch(&file, &mut target, &attachment, &tx, &counters, true);
    assert!(target.as_ref().unwrap().fingerprint.is_none());
}

#[test]
fn legacy_project_sources_keep_actual_file_paths() {
    for (directory, harness) in [
        (".opencode", crate::harness::Harness::Opencode),
        (".pi", crate::harness::Harness::Pi),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(directory).join("aft.jsonc");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let text = "{\"format_on_edit\":false}";
        std::fs::write(&path, text).unwrap();
        let config = crate::config::Config {
            harness: Some(harness),
            ..Default::default()
        };
        let tiers = [crate::config_resolve::ConfigTier {
            tier: "project".to_string(),
            source: path.to_string_lossy().into_owned(),
            doc: text.to_string(),
        }];
        let sources = super::ConfigSources::from_configure(None, dir.path(), &tiers, &config);
        assert_eq!(sources.project.path.as_deref(), Some(path.as_path()));
        assert!(sources.project.from_file);
        assert_eq!(sources.project.text.as_deref(), Some(text));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        assert!(!dir.path().join(".cortexkit/aft.jsonc").exists());
    }
}

/// Stopping a config directory watch must never stall another thread's file
/// I/O. notify 8's Windows backend could re-arm `ReadDirectoryChangesW` from
/// a completion that arrived just before its stop cancelled the read, on the
/// directory handle it had just closed. Windows may reuse a freed handle value
/// for a later handle opened by any thread. When that was a synchronous handle on
/// a quiet directory, the re-armed read blocked inside the kernel on it, and
/// the owner's next call on that handle waited behind the read forever. A
/// test whose `TempDir` removal listed directories while a parent folder's
/// config watch stopped hung that way.
///
/// `assert_watch_restarts_never_stall_file_io` keeps changing the watched
/// directory so reads complete all the time and keeps opening synchronous
/// handles on a quiet directory, while this test starts and stops the watch.
#[cfg(windows)]
#[test]
fn stopping_a_config_directory_watch_never_stalls_other_file_io() {
    let counters = Arc::new(crate::context::WatcherCounters::default());
    crate::watcher_backend::assert_watch_restarts_never_stall_file_io(
        "config directory",
        move |watched| {
            let (tx, rx) = mpsc::channel();
            let mut watcher =
                new_config_directory_watcher(tx, Arc::clone(&counters)).expect("watcher");
            watcher
                .watch(watched, notify::RecursiveMode::NonRecursive)
                .expect("watch");
            let _ = rx.recv_timeout(Duration::from_millis(2));
            let _ = watcher.unwatch(watched);
        },
    );
}
