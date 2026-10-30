use super::*;
use notify::event::{CreateKind, ModifyKind, RemoveKind};
use std::process::Command;

fn filesystem_activity_lease() -> std::sync::MutexGuard<'static, ()> {
    // Keep quiet-project measurements separate from other large fixtures. The
    // saturation regression still runs its own concurrent 50k-file writer.
    static ACTIVITY: Mutex<()> = Mutex::new(());
    ACTIVITY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn git(root: &Path, args: &[&str]) {
    assert!(Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .status()
        .unwrap()
        .success());
}
fn repo() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    git(root.path(), &["init", "-q"]);
    git(
        root.path(),
        &["config", "user.email", "test@example.invalid"],
    );
    git(root.path(), &["config", "user.name", "Test"]);
    std::fs::write(root.path().join("clean.txt"), "old\n").unwrap();
    std::fs::write(root.path().join("deleted.txt"), "gone\n").unwrap();
    git(root.path(), &["add", "."]);
    git(root.path(), &["commit", "-qm", "initial"]);
    root
}
fn event(journal: &Journal, root: &Path, name: &str, kind: notify::EventKind) {
    journal.event(&notify::Event::new(kind).add_path(root.join(name)));
}
fn capture(root: &Path, mode: BashChangedFiles) -> (Arc<Journal>, Arc<Capture>) {
    let journal = Arc::new(Journal::default());
    // These tests check what the report says when git answers, not how fast
    // git starts; give the before-image capture an ample budget.
    TEST_GIT_CAPTURE_BUDGET.with(|slot| slot.set(Some(Duration::from_secs(30))));
    let mut capture = Capture::begin(root.to_path_buf(), journal.clone(), mode, true);
    TEST_GIT_CAPTURE_BUDGET.with(|slot| slot.set(None));
    if mode == BashChangedFiles::Diff {
        let config = crate::config::Config::default();
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
            .unwrap_or_default();
        let access = crate::access_policy::AccessPolicy::from_config(
            &config,
            &root.join(".test-aft-storage"),
            vec![root.canonicalize().unwrap()],
            &home,
        );
        Arc::get_mut(&mut capture).unwrap().read_policy = Some(ReadPolicy::new(access, None));
    }
    // Likewise give rendering an ample budget: a loaded runner's slow Git
    // startup must not turn a content check into a deadline check. Tests of
    // deadline behaviour set a call deadline with `Capture::budget`.
    Arc::get_mut(&mut capture).unwrap().render_time_for_test = Duration::from_secs(30);
    (journal, capture)
}
#[test]
fn bash_changed_files_list_and_diff_in_real_git_repo() {
    let root = repo();
    for mode in [BashChangedFiles::List, BashChangedFiles::Diff] {
        let (journal, capture) = capture(root.path(), mode);
        std::fs::write(root.path().join("clean.txt"), "new\n").unwrap();
        event(
            &journal,
            root.path(),
            "clean.txt",
            notify::EventKind::Modify(ModifyKind::Any),
        );
        let report = capture.finish();
        assert_eq!(report.files[0].path, "clean.txt");
        assert_eq!(report.files[0].status, "modified");
        assert!(report.text.starts_with(HEADER));
        if mode == BashChangedFiles::Diff {
            let diff = report.files[0].diff.as_deref().unwrap();
            assert!(diff.contains("-old\n+new"), "{diff}");
        } else {
            assert!(report.files[0].diff.is_none());
        }
        git(root.path(), &["checkout", "--", "clean.txt"]);
    }
}
#[test]
fn bash_changed_files_already_dirty_and_existing_untracked_have_no_diff() {
    let root = repo();
    std::fs::write(root.path().join("clean.txt"), "dirty before\n").unwrap();
    std::fs::write(root.path().join("untracked.txt"), "before\n").unwrap();
    let (journal, capture) = capture(root.path(), BashChangedFiles::Diff);
    for name in ["clean.txt", "untracked.txt"] {
        std::fs::write(root.path().join(name), "after\n").unwrap();
        event(
            &journal,
            root.path(),
            name,
            notify::EventKind::Create(CreateKind::File),
        );
    }
    let report = capture.finish();
    assert_eq!(report.files.len(), 2);
    assert!(
        report
            .files
            .iter()
            .all(|file| file.diff.is_none() && file.reason.as_deref() == Some(DIRTY)),
        "{report:?}; git={:?}",
        capture.git
    );
}
#[test]
fn bash_changed_files_deleted_new_binary_and_large() {
    let root = repo();
    let (journal, capture) = capture(root.path(), BashChangedFiles::Diff);
    std::fs::remove_file(root.path().join("deleted.txt")).unwrap();
    event(
        &journal,
        root.path(),
        "deleted.txt",
        notify::EventKind::Remove(RemoveKind::File),
    );
    for (name, bytes) in [
        ("new.txt", b"added\n".to_vec()),
        ("binary", vec![0, 1, 255]),
        ("large", vec![b'x'; MAX_FILE_BYTES + 1]),
    ] {
        std::fs::write(root.path().join(name), bytes).unwrap();
        event(
            &journal,
            root.path(),
            name,
            notify::EventKind::Create(CreateKind::File),
        );
    }
    let report = capture.finish();
    let find = |name| report.files.iter().find(|file| file.path == name).unwrap();
    assert!(find("deleted.txt")
        .diff
        .as_ref()
        .unwrap_or_else(|| panic!("missing deleted diff: {report:?}; git={:?}", capture.git))
        .contains("-gone"));
    assert_eq!(find("deleted.txt").status, "deleted");
    assert!(find("deleted.txt")
        .diff
        .as_ref()
        .unwrap()
        .contains("+++ /dev/null"));
    assert!(find("new.txt").diff.as_ref().unwrap().contains("+added"));
    assert_eq!(find("new.txt").status, "added");
    assert!(find("new.txt")
        .diff
        .as_ref()
        .unwrap()
        .contains("--- /dev/null"));
    assert_eq!(
        find("binary").reason.as_deref(),
        Some("no diff: binary file")
    );
    assert!(find("large").reason.as_ref().unwrap().contains("too large"));
}

#[cfg(any(windows, target_os = "macos"))]
#[test]
fn bash_changed_files_git_aliases_preserve_dirty_untracked_and_deleted_paths() {
    let root = repo();
    let canonical = root.path().canonicalize().unwrap();
    #[cfg(target_os = "macos")]
    let alias_parent = tempfile::tempdir().unwrap();
    #[cfg(target_os = "macos")]
    let capture_root = if let Ok(relative) = canonical.strip_prefix("/private") {
        Path::new("/").join(relative)
    } else {
        let alias = alias_parent.path().join("project-alias");
        std::os::unix::fs::symlink(&canonical, &alias).unwrap();
        alias
    };
    #[cfg(windows)]
    let capture_root = root.path().to_path_buf();
    std::fs::write(root.path().join("clean.txt"), "dirty before\n").unwrap();
    std::fs::write(root.path().join("existing.txt"), "untracked before\n").unwrap();
    let (journal, mut capture) = capture(&capture_root, BashChangedFiles::Diff);
    let git = Arc::get_mut(&mut capture).unwrap().git.as_mut().unwrap();
    #[cfg(windows)]
    {
        let original = git.directory.clone();
        git.directory = PathBuf::from(canonical.to_string_lossy().to_ascii_uppercase());
        git.dirty = git
            .dirty
            .iter()
            .map(|path| git.directory.join(path.strip_prefix(&original).unwrap()))
            .collect();
    }
    let original_directory = git.directory.clone();
    let original_dirty = git.dirty.clone();
    for (name, contents) in [
        ("clean.txt", "dirty after\n"),
        ("existing.txt", "untracked after\n"),
        ("new.txt", "new file\n"),
    ] {
        std::fs::write(root.path().join(name), contents).unwrap();
        event(
            &journal,
            &capture_root,
            name,
            notify::EventKind::Modify(ModifyKind::Any),
        );
    }
    std::fs::remove_file(root.path().join("deleted.txt")).unwrap();
    event(
        &journal,
        &capture_root,
        "deleted.txt",
        notify::EventKind::Remove(RemoveKind::File),
    );
    let report = capture.finish();
    let find = |name| report.files.iter().find(|file| file.path == name).unwrap();
    for name in ["clean.txt", "existing.txt"] {
        assert_eq!(find(name).reason.as_deref(), Some(DIRTY), "{report:?}");
        assert!(find(name).diff.is_none());
    }
    assert!(
        find("new.txt")
            .diff
            .as_ref()
            .is_some_and(|diff| diff.contains("+new file")),
        "{report:?}"
    );
    assert!(
        find("deleted.txt")
            .diff
            .as_ref()
            .is_some_and(|diff| diff.contains("-gone")),
        "{report:?}"
    );
    assert_eq!(capture.root, capture_root);
    assert_eq!(capture.git.as_ref().unwrap().directory, original_directory);
    assert_eq!(capture.git.as_ref().unwrap().dirty, original_dirty);
}

#[test]
fn bash_changed_files_diff_deadline_is_not_reported_as_missing_before_image() {
    // `show` fails fast because HEAD lacks the new file; `ls-tree` then either
    // answers, fails, or is stopped at the deadline. Only a real answer may
    // decide between "added" and "no before-image".
    for (ls_tree, expected) in [
        ("empty", None),
        ("fails", Some(NO_BEFORE_IMAGE)),
        ("times out", Some(DIFF_BUDGET_SPENT)),
    ] {
        let root = repo();
        let (journal, capture) = capture(root.path(), BashChangedFiles::Diff);
        std::fs::write(root.path().join("new.txt"), "new file\n").unwrap();
        event(
            &journal,
            root.path(),
            "new.txt",
            notify::EventKind::Modify(ModifyKind::Any),
        );
        GIT_FOR_TEST.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move |args, deadline| match args[0] {
                "show" => None,
                "ls-tree" => match ls_tree {
                    "empty" => Some(Vec::new()),
                    "fails" => None,
                    _ => {
                        // Like `bounded_git_bytes`, give up only once the
                        // deadline has passed.
                        std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
                        None
                    }
                },
                other => panic!("unexpected git {other}"),
            }));
        });
        // Only the timeout case needs the deadline to pass; the cases where
        // `ls-tree` answers get a budget a loaded runner cannot spend before
        // `ls-tree` runs, so a slow `finish` can't turn them into a timeout.
        let budget = if expected == Some(DIFF_BUDGET_SPENT) {
            Duration::from_millis(500)
        } else {
            Duration::from_secs(30)
        };
        let _budget = capture.budget(Some(Instant::now() + budget));
        let report = capture.finish();
        GIT_FOR_TEST.with(|hook| *hook.borrow_mut() = None);
        let file = &report.files[0];
        assert_eq!(file.path, "new.txt");
        assert_eq!(
            file.reason.as_deref(),
            expected,
            "ls-tree {ls_tree}: {report:?}"
        );
        if expected.is_none() {
            assert_eq!(file.status, "added", "{report:?}");
            assert!(
                file.diff.as_ref().is_some_and(
                    |diff| diff.contains("--- /dev/null") && diff.contains("+new file")
                ),
                "{report:?}"
            );
        } else {
            assert!(file.diff.is_none(), "{report:?}");
        }
    }
}

#[test]
fn bash_changed_files_skipped_diffs_say_the_budget_ran_out() {
    let root = repo();
    let (journal, mut capture) = capture(root.path(), BashChangedFiles::Diff);
    Arc::get_mut(&mut capture).unwrap().diff_delay_for_test = Duration::from_secs(5);
    for name in ["clean.txt", "new.txt"] {
        std::fs::write(root.path().join(name), "changed\n").unwrap();
        event(
            &journal,
            root.path(),
            name,
            notify::EventKind::Modify(ModifyKind::Any),
        );
    }
    // The first diff sleeps through the whole budget; the second is never
    // attempted. Neither may look like a file that simply has no diff.
    let _budget = capture.budget(Some(Instant::now() + Duration::from_millis(200)));
    let report = capture.finish();
    assert_eq!(report.files.len(), 2, "{report:?}");
    for file in &report.files {
        assert!(file.diff.is_none(), "{report:?}");
        assert_eq!(
            file.reason.as_deref(),
            Some(DIFF_BUDGET_SPENT),
            "{report:?}"
        );
    }
}

#[test]
fn bash_changed_files_caps_files_lines_bytes_and_overflow() {
    let root = repo();
    let (journal, capture) = capture(root.path(), BashChangedFiles::Diff);
    for i in 0..MAX_FILES + 4 {
        let name = format!("new-{i:03}.txt");
        std::fs::write(root.path().join(&name), "x\n".repeat(500)).unwrap();
        event(
            &journal,
            root.path(),
            &name,
            notify::EventKind::Create(CreateKind::File),
        );
    }
    journal.gap();
    let report = capture.finish();
    assert!(report.files.len() <= MAX_FILES);
    assert!(report.text.len() <= MAX_REPORT_BYTES);
    assert!(serde_json::to_vec(report).unwrap().len() <= MAX_REPORT_BYTES);
    assert!(
        report.text.contains("shown 200 of"),
        "{report:?}; git={:?}",
        capture.git
    );
    assert!(report.text.contains(OVERFLOW));
    assert_eq!(report.changed_files_list_envelope.reason, Some(Reason::Cap));
    assert!(report.changed_files_list_envelope.total.is_at_least());
    // Expensive Git process startup can exhaust the diff budget before all
    // listed files get a diff. Every produced diff must still carry its cap.
    assert!(report.files.iter().all(|file| match &file.diff {
        Some(_) => file
            .diff_lines_list_envelope
            .as_ref()
            .is_some_and(|envelope| envelope.shown == MAX_DIFF_LINES),
        None => file.diff_lines_list_envelope.is_none(),
    }));
}
#[test]
fn bash_changed_files_ring_overrun_and_outside_git_are_honest() {
    let root = tempfile::tempdir().unwrap();
    let (journal, capture) = capture(root.path(), BashChangedFiles::Diff);
    for _ in 0..MAX_EVENTS + 2 {
        event(
            &journal,
            root.path(),
            "note.txt",
            notify::EventKind::Create(CreateKind::File),
        );
    }
    let report = capture.finish();
    assert!(report.text.contains(OVERFLOW));
    assert!(report.text.contains(OUTSIDE_GIT));
    assert!(report.files[0].diff.is_none());
    assert_eq!(capture.finish().text, report.text);
}
#[test]
fn bash_changed_files_off_does_no_probes_or_watcher_registration() {
    let root = tempfile::tempdir().unwrap();
    let mut config = crate::config::Config::default();
    config.project_root = Some(root.path().to_path_buf());
    let ctx =
        crate::context::AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config);
    assert_eq!(ctx.config().bash.changed_files, BashChangedFiles::Off);
    assert!(Capture::start(&ctx).is_none());
    assert!(ctx.watcher_counters().bash_changes.get().is_none());
}

#[test]
fn bash_changed_files_git_capture_combines_metadata_and_shares_budget() {
    let mut calls = Vec::new();
    let mut deadlines = Vec::new();
    let head = "a".repeat(40);
    let directory = "captured root\nwith newline";
    let state = GitStart::read_with_git(
        Path::new("project"),
        Duration::from_secs(3600),
        |_, args, deadline| {
            calls.push(
                args.iter()
                    .map(|arg| (*arg).to_string())
                    .collect::<Vec<_>>(),
            );
            deadlines.push(deadline);
            match calls.len() {
                1 => {
                    assert_eq!(args, ["rev-parse", "--show-toplevel", "--verify", "HEAD"]);
                    Some(format!("{directory}\n{head}\n").into_bytes())
                }
                2 => {
                    assert_eq!(args[0], "status");
                    Some(b" M clean.txt\0?? existing.txt\0".to_vec())
                }
                _ => panic!("unexpected extra Git process: {args:?}"),
            }
        },
    )
    .unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(deadlines[0], deadlines[1]);
    assert_eq!(state.directory, PathBuf::from(directory));
    assert_eq!(state.head, head);
    assert_eq!(
        state.dirty,
        BTreeSet::from([
            Path::new(directory).join("clean.txt"),
            Path::new(directory).join("existing.txt")
        ])
    );
}

#[test]
fn bash_changed_files_expired_git_capture_budget_starts_no_process() {
    let mut calls = 0;
    let state = GitStart::read_with_git(Path::new("project"), Duration::ZERO, |_, _, _| {
        calls += 1;
        None
    });
    assert!(state.is_none());
    assert_eq!(calls, 0);
}

#[test]
#[ignore = "manual 50k-file latency measurement; run cargo test -p agent-file-tools --lib -- bash_changed_files_50k_fixture_start_latency --ignored --nocapture --test-threads=1"]
fn bash_changed_files_50k_fixture_start_latency() {
    let _activity = filesystem_activity_lease();
    let fixture_start = Instant::now();
    let root = repo();
    let many = root.path().join("many");
    std::fs::create_dir(&many).unwrap();
    for i in 0..50_000 {
        std::fs::write(many.join(format!("file-{i}")), "fixture\n").unwrap();
    }
    git(root.path(), &["add", "."]);
    git(root.path(), &["commit", "-qm", "50k files"]);
    let fixture_elapsed = fixture_start.elapsed();
    let start = Instant::now();
    let (_, list) = capture(root.path(), BashChangedFiles::List);
    let list_elapsed = start.elapsed();
    let start = Instant::now();
    let (_, diff) = capture(root.path(), BashChangedFiles::Diff);
    let diff_elapsed = start.elapsed();
    eprintln!("bash changed-files 50k fixture: fixture_build={fixture_elapsed:?}, list_start={list_elapsed:?}, git_capture_start={diff_elapsed:?}, git_available={}", diff.git.is_some());
    assert!(list.git.is_none());
}

#[test]
#[ignore = "manual tiny-repo Git startup measurement; run cargo test -p agent-file-tools --lib -- bash_changed_files_small_repo_git_capture_latency --ignored --nocapture --test-threads=1"]
fn bash_changed_files_small_repo_git_capture_latency() {
    let root = repo();
    let mut times = Vec::new();
    let mut captured = 0;
    for _ in 0..10 {
        let start = Instant::now();
        captured += usize::from(GitStart::read(root.path()).is_some());
        times.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    eprintln!(
        "tiny Git capture os={} samples={} captured={} mean_ms={:.3} max_ms={:.3}",
        std::env::consts::OS,
        times.len(),
        captured,
        times.iter().sum::<f64>() / times.len() as f64,
        times.iter().copied().fold(0.0_f64, f64::max),
    );
}

#[test]
fn bash_changed_files_sealed_window_excludes_later_events_and_overflow() {
    let root = tempfile::tempdir().unwrap();
    let (journal, capture) = capture(root.path(), BashChangedFiles::List);
    event(
        &journal,
        root.path(),
        "during.txt",
        notify::EventKind::Create(CreateKind::File),
    );
    capture.seal();
    event(
        &journal,
        root.path(),
        "after.txt",
        notify::EventKind::Create(CreateKind::File),
    );
    journal.gap();
    let report = capture.finish();
    assert_eq!(report.files.len(), 1);
    assert_eq!(report.files[0].path, "during.txt");
    assert!(!report.text.contains(OVERFLOW));
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
struct NativeWatcher {
    watcher: Option<crate::watcher_backend::ProjectWatcher>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
    counters: Arc<crate::context::WatcherCounters>,
    journal: Arc<Journal>,
    markers: Arc<Mutex<Vec<PathBuf>>>,
    generated: Arc<std::sync::atomic::AtomicUsize>,
    generated_ready: std::sync::mpsc::Receiver<()>,
    delay_last_write: Arc<std::sync::atomic::AtomicBool>,
    root: PathBuf,
}
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
impl NativeWatcher {
    fn new(root: &Path, watch_git: bool) -> Self {
        use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
        let root = root.canonicalize().unwrap();
        let counters = crate::context::watcher_counters_for_root(&root);
        let journal = counters
            .bash_changes
            .get_or_init(|| Arc::new(Journal::default()))
            .clone();
        let mut ignores = ignore::gitignore::GitignoreBuilder::new(&root);
        ignores.add_line(None, "/target/").unwrap();
        if root.join(".gitignore").is_file() {
            assert!(ignores.add(root.join(".gitignore")).is_none());
        }
        let matcher = Arc::new(std::sync::RwLock::new(Some(Arc::new(
            ignores.build().unwrap(),
        ))));
        let (tx, rx) = std::sync::mpsc::channel();
        let observed_matcher = matcher.clone();
        let observed_root = root.clone();
        let watcher = crate::watcher_backend::create_project_watcher(
            root.clone(),
            if watch_git {
                vec![root.join(".git")]
            } else {
                vec![]
            },
            tx,
            matcher,
            Arc::new(AtomicU64::new(0)),
        )
        .unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let observed = counters.clone();
        let observed_journal = journal.clone();
        let markers = Arc::new(Mutex::new(Vec::new()));
        let recorded_markers = markers.clone();
        let generated = Arc::new(AtomicUsize::new(0));
        let (generated_tx, generated_ready) = std::sync::mpsc::sync_channel(1);
        let generated_events = generated.clone();
        let delay_last_write = Arc::new(AtomicBool::new(false));
        let delayed_consumer = delay_last_write.clone();
        let join = std::thread::spawn(move || {
            while !stopped.load(Ordering::Acquire) {
                if let Ok(Ok(event)) = rx.recv_timeout(Duration::from_millis(10)) {
                    if delayed_consumer.load(Ordering::Acquire)
                        && event.paths.iter().any(|path| {
                            path.file_name()
                                .is_some_and(|name| name == "last-write.txt")
                        })
                    {
                        // Simulate delayed intake without dropping native events. A
                        // native flush alone must not stand in for journal delivery.
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    for path in &event.paths {
                        if crate::watcher_backend::flush::is_marker_path(path) {
                            recorded_markers.lock().unwrap().push(path.clone());
                        }
                        if path
                            .components()
                            .any(|part| part.as_os_str() == "generated")
                        {
                            generated_events.fetch_add(1, Ordering::Relaxed);
                            let _ = generated_tx.try_send(());
                        }
                    }
                    if observed.bash_flush.observe(&event) {
                        continue;
                    }
                    if event.need_rescan() {
                        observed_journal.gap();
                    }
                    observed_journal.event_for_root(&event, &observed_root, &observed_matcher);
                }
            }
        });
        Self {
            watcher: Some(watcher),
            stop,
            join: Some(join),
            counters,
            journal,
            markers,
            generated,
            generated_ready,
            delay_last_write,
            root,
        }
    }
    fn capture(&self) -> Arc<Capture> {
        let mut capture = Capture::begin(
            self.root.clone(),
            self.journal.clone(),
            BashChangedFiles::List,
            true,
        );
        Arc::get_mut(&mut capture).unwrap().flush = Some(self.counters.bash_flush.clone());
        capture
    }
    fn ready(&self) {
        // Quiet-project measurements begin after the native service has
        // acknowledged startup. The saturation regression deliberately does not.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !self.counters.bash_flush.wait(
            &self.root,
            (Instant::now() + crate::watcher_backend::flush::BASH_FLUSH_LIMIT).min(deadline),
        ) {
            assert!(
                Instant::now() < deadline,
                "native watcher did not become ready"
            );
        }
    }
}
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
impl Drop for NativeWatcher {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        drop(self.watcher.take());
        self.join.take().unwrap().join().unwrap();
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "requires a live FSEvents service; run the focused native gate separately from large fixtures"
)]
fn bash_changed_files_flush_native_catches_exit_write() {
    let _activity = filesystem_activity_lease();
    let root = tempfile::tempdir().unwrap();
    let native = NativeWatcher::new(root.path(), false);
    native.ready();
    for i in 0..5 {
        let capture = native.capture();
        let name = format!("last-write-{i}.txt");
        std::fs::write(root.path().join(&name), "last write before exit\n").unwrap();
        let report = capture.finish();
        assert!(
            report.files.iter().any(|file| file.path == name),
            "{report:?}; root={:?}; journal={:?}",
            native.root,
            native.journal.state.lock().unwrap()
        );
        assert!(report.incomplete.is_empty(), "{report:?}");
        assert!(!report
            .files
            .iter()
            .any(|file| file.path.contains("aft-flush")));
    }
    assert!(native
        .markers
        .lock()
        .unwrap()
        .iter()
        .all(|path| !path.exists()));
}

#[cfg(any(target_os = "linux", windows))]
#[test]
fn bash_changed_files_flush_native_git_directory_and_linked_worktree_markers() {
    let root = repo();
    let native = NativeWatcher::new(root.path(), true);
    assert!(native.counters.bash_flush.wait(
        &native.root,
        Instant::now() + crate::watcher_backend::flush::BASH_FLUSH_LIMIT
    ));
    let markers = native.markers.lock().unwrap();
    assert!(!markers.is_empty());
    assert!(markers
        .iter()
        .all(|path| path.parent() == Some(native.root.join(".git").as_path()) && !path.exists()));
    drop(markers);
    drop(native);
    let linked_parent = tempfile::tempdir().unwrap();
    let linked = linked_parent.path().join("linked");
    git(
        root.path(),
        &[
            "worktree",
            "add",
            "-qb",
            "flush-linked",
            linked.to_str().unwrap(),
        ],
    );
    assert!(linked.join(".git").is_file());
    let native = NativeWatcher::new(&linked, false);
    assert!(native.counters.bash_flush.wait(
        &native.root,
        Instant::now() + crate::watcher_backend::flush::BASH_FLUSH_LIMIT
    ));
    let markers = native.markers.lock().unwrap();
    assert!(!markers.is_empty());
    assert!(markers
        .iter()
        .all(|path| path.parent() == Some(native.root.as_path()) && !path.exists()));
}

#[test]
#[ignore = "manual flush deadline timing; run cargo test -p agent-file-tools --lib -- bash_changed_files_flush_timeout_and_remaining_call_budget --ignored --nocapture --test-threads=1"]
fn bash_changed_files_flush_timeout_and_remaining_call_budget() {
    let root = tempfile::tempdir().unwrap();
    let (journal, mut capture) = capture(root.path(), BashChangedFiles::List);
    let flush = Arc::new(crate::watcher_backend::flush::FlushControl::default());
    Arc::get_mut(&mut capture).unwrap().flush = Some(flush.clone());
    let _budget = capture.budget(Some(Instant::now() + Duration::from_millis(20)));
    event(
        &journal,
        root.path(),
        "changed.txt",
        notify::EventKind::Create(CreateKind::File),
    );
    let start = Instant::now();
    let report = capture.finish();
    assert_eq!(report.incomplete, vec![NOT_CAUGHT_UP]);
    assert!(report.changed_files_list_envelope.total.is_at_least());
    assert!(start.elapsed() < Duration::from_millis(150));
    assert!(!root
        .path()
        .read_dir()
        .unwrap()
        .any(|entry| crate::watcher_backend::flush::is_marker_path(&entry.unwrap().path())));
    assert!(!flush.wait(
        root.path(),
        Instant::now() + crate::watcher_backend::flush::BASH_FLUSH_LIMIT
    ));
    assert!(!root
        .path()
        .read_dir()
        .unwrap()
        .any(|entry| crate::watcher_backend::flush::is_marker_path(&entry.unwrap().path())));
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
#[test]
#[ignore = "manual native build/flush latency measurement; run cargo test -p agent-file-tools --lib -- bash_changed_files_flush_native_latency_during_build --ignored --nocapture --test-threads=1"]
fn bash_changed_files_flush_native_latency_during_build() {
    let _activity = filesystem_activity_lease();
    use std::sync::atomic::Ordering;
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("src")).unwrap();
    std::fs::write(
        root.path().join("Cargo.toml"),
        "[package]\nname='flush-build-fixture'\nversion='0.1.0'\nedition='2021'\n[workspace]\n",
    )
    .unwrap();
    std::fs::write(
        root.path().join("src/lib.rs"),
        "include!(\"generated.rs\");\n",
    )
    .unwrap();
    std::fs::write(root.path().join("build.rs"), r##"fn main() {
        std::fs::create_dir_all("src/generated").unwrap();
        let mut modules = String::new();
        for i in 0..2000 {
            std::fs::write(format!("src/generated/m{i}.rs"), format!("pub const VALUE: usize = {i};\n")).unwrap();
            modules.push_str(&format!("#[path=\"generated/m{i}.rs\"] pub mod m{i};\n"));
            // Sustain source writes without synthetic CPU load during sampling.
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        std::fs::write("src/generated.rs", modules).unwrap();
        println!("cargo:rerun-if-changed=build.rs");
    }"##).unwrap();
    let native = NativeWatcher::new(root.path(), false);
    native.ready();
    let measure = || {
        let start = Instant::now();
        let caught_up = native.counters.bash_flush.wait(
            &native.root,
            start + crate::watcher_backend::flush::BASH_FLUSH_LIMIT,
        );
        (start.elapsed(), caught_up)
    };
    let quiet = (0..10).map(|_| measure()).collect::<Vec<_>>();
    let mut child = Command::new("cargo")
        .args(["check", "--offline", "--quiet"])
        .current_dir(root.path())
        .env("CARGO_TARGET_DIR", root.path().join("target"))
        .env("RUSTC_WRAPPER", "")
        .env("CARGO_BUILD_RUSTC_WRAPPER", "")
        .spawn()
        .unwrap();
    let mut busy = Vec::new();
    native
        .generated_ready
        .recv_timeout(Duration::from_secs(30))
        .expect("build started source generation");
    let deadline = Instant::now() + Duration::from_secs(60);
    while child.try_wait().unwrap().is_none() {
        let before = native.generated.load(Ordering::Relaxed);
        let measurement = measure();
        if native.generated.load(Ordering::Relaxed) != before {
            busy.push(measurement);
        }
        if busy.len() == 20 {
            break;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("fixture build exceeded deadline");
        }
    }
    assert!(child.wait().unwrap().success());
    assert!(
        !busy.is_empty(),
        "no barrier overlapped generated-source writes"
    );
    let summary = |values: &[(Duration, bool)]| {
        let sum = values
            .iter()
            .map(|(time, _)| time.as_secs_f64() * 1000.0)
            .sum::<f64>();
        let max = values
            .iter()
            .map(|(time, _)| time.as_secs_f64() * 1000.0)
            .fold(0.0_f64, f64::max);
        format!(
            "n={} mean_ms={:.3} max_ms={:.3} timeouts={}",
            values.len(),
            sum / values.len() as f64,
            max,
            values.iter().filter(|(_, ok)| !ok).count()
        )
    };
    eprintln!(
        "bash flush barrier {} quiet [{}] active_cargo_source_generation [{}]",
        std::env::consts::OS,
        summary(&quiet),
        summary(&busy)
    );
    assert!(quiet.iter().all(|(_, ok)| *ok));
}

#[test]
fn bash_changed_files_flush_markers_are_excluded_from_project_walks() {
    let root = tempfile::tempdir().unwrap();
    for name in [".aft-flush-123-1", ".aft-flush-not-private", "visible.rs"] {
        std::fs::write(root.path().join(name), "pub const VALUE: usize = 1;\n").unwrap();
    }
    let marker = root.path().join(".aft-flush-123-1");
    assert!(crate::project_content::is_excluded(&marker));
    assert!(!crate::project_content::is_excluded(
        &root.path().join(".aft-flush-not-private")
    ));
    let paths = crate::search_index::project_walk_builder(root.path())
        .build()
        .filter_map(Result::ok)
        .map(|entry| entry.path().to_path_buf())
        .collect::<Vec<_>>();
    assert!(!paths.contains(&marker));
    assert!(paths.contains(&root.path().join("visible.rs")));
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "requires a live FSEvents service and concurrent filesystem activity"
)]
fn bash_changed_files_flush_native_no_false_complete_during_50k_activity() {
    let _activity = filesystem_activity_lease();
    let root = tempfile::tempdir().unwrap();
    let activity = tempfile::tempdir().unwrap();
    let activity_path = activity.path().to_path_buf();
    // Delayed journal intake below exercises the flush/delivery race even when
    // the smaller default writer does not saturate the native event service.
    let files = if std::env::var("AFT_STRESS").as_deref() == Ok("1") {
        50_000
    } else {
        1_000
    };
    let (started_tx, started_rx) = std::sync::mpsc::sync_channel(0);
    let writer = std::thread::spawn(move || {
        for i in 0..files {
            std::fs::write(activity_path.join(format!("fixture-{i}")), "fixture\n").unwrap();
            if i == 100 {
                started_tx.send(()).unwrap();
            }
        }
    });
    started_rx.recv().unwrap();
    let native = NativeWatcher::new(root.path(), false);
    native
        .delay_last_write
        .store(true, std::sync::atomic::Ordering::Release);
    let capture = native.capture();
    std::fs::write(
        root.path().join("last-write.txt"),
        "written immediately before completion\n",
    )
    .unwrap();
    let report = capture.finish();
    writer.join().unwrap();
    if report.incomplete.is_empty() {
        assert!(
            report
                .files
                .iter()
                .any(|file| file.path == "last-write.txt"),
            "false complete: {report:?}"
        );
    } else {
        // Under service saturation a bounded timeout is honest; a successful
        // flush must never silently omit the write, as FlushSync alone could.
        assert_eq!(report.incomplete, vec![NOT_CAUGHT_UP], "{report:?}");
    }
}

#[test]
fn bash_changed_files_flush_inherits_reply_budget_before_spawn_and_clears_on_handoff() {
    let root = tempfile::tempdir().unwrap();
    let mut config = crate::config::Config::default();
    config.project_root = Some(root.path().into());
    config.bash.changed_files = BashChangedFiles::List;
    let ctx =
        crate::context::AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config);
    let deadline = Instant::now() + Duration::from_millis(20);
    let budget = BudgetGuard::new(deadline);
    let receipt = Arc::new(crate::bash_background::SpawnReceipt::new(
        Instant::now() + Duration::from_secs(30),
    ));
    receipt.bind_changed_files_deadline(budget.shared());
    let capture = crate::bash_background::with_spawn_receipt(receipt.clone(), || {
        Capture::start(&ctx).unwrap()
    });
    assert_eq!(*capture.call_deadline.lock().unwrap(), Some(deadline));
    receipt.limit_changed_files_deadline(deadline + Duration::from_secs(1));
    assert_eq!(*capture.call_deadline.lock().unwrap(), Some(deadline));
    drop(budget);
    assert!(capture.call_deadline.lock().unwrap().is_none());
    receipt.limit_changed_files_deadline(deadline);
    assert!(
        capture.call_deadline.lock().unwrap().is_none(),
        "handoff cannot revive a caller budget"
    );
    let late =
        crate::bash_background::with_spawn_receipt(receipt, || Capture::start(&ctx).unwrap());
    assert!(late.call_deadline.lock().unwrap().is_none());
}

#[test]
#[ignore = "manual render deadline timing; run cargo test -p agent-file-tools --lib -- bash_changed_files_diff_rendering_respects_reply_deadline --ignored --nocapture --test-threads=1"]
fn bash_changed_files_diff_rendering_respects_reply_deadline() {
    let root = repo();
    let (journal, mut capture) = capture(root.path(), BashChangedFiles::Diff);
    assert!(capture.git.is_some(), "real git before-images are required");
    Arc::get_mut(&mut capture).unwrap().diff_delay_for_test = Duration::from_millis(200);
    for name in ["clean.txt", "deleted.txt"] {
        std::fs::write(root.path().join(name), "changed\n").unwrap();
        event(
            &journal,
            root.path(),
            name,
            notify::EventKind::Modify(ModifyKind::Any),
        );
    }
    let started = Instant::now();
    let _budget = capture.budget(Some(started + Duration::from_millis(30)));
    let report = capture.finish();
    assert!(
        started.elapsed() < Duration::from_millis(120),
        "render overran reply budget: {:?}",
        started.elapsed()
    );
    assert_eq!(
        report.files.len(),
        2,
        "remaining paths should still be listed"
    );
    assert!(report.files.iter().all(|file| file.diff.is_none()));
    assert!(
        report.incomplete.is_empty(),
        "a complete path list needs no new caveat"
    );
}

#[test]
fn bash_changed_files_expired_render_budget_marks_cut_list_incomplete() {
    let root = tempfile::tempdir().unwrap();
    let (journal, capture) = capture(root.path(), BashChangedFiles::List);
    std::fs::write(root.path().join("changed.txt"), "changed").unwrap();
    event(
        &journal,
        root.path(),
        "changed.txt",
        notify::EventKind::Create(CreateKind::File),
    );
    let _budget = capture.budget(Some(Instant::now() - Duration::from_millis(1)));
    let report = capture.finish();
    assert!(report.files.is_empty());
    assert_eq!(report.incomplete, vec![NOT_CAUGHT_UP]);
    assert_eq!(
        report.changed_files_list_envelope.reason,
        Some(Reason::TimeLimit)
    );
    assert!(report.changed_files_list_envelope.total.is_at_least());
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "requires a live FSEvents service; run as a focused native gate"
)]
fn bash_changed_files_ignored_paths_are_not_reported() {
    let root = repo();
    std::fs::write(root.path().join(".gitignore"), "config.local.json\n").unwrap();
    let native = NativeWatcher::new(root.path(), false);
    native.ready();
    let capture = native.capture();
    std::fs::write(
        root.path().join("config.local.json"),
        "ignored command output\n",
    )
    .unwrap();
    std::fs::write(root.path().join("clean.txt"), "tracked command output\n").unwrap();
    let report = capture.finish();
    assert!(
        report.files.iter().any(|file| file.path == "clean.txt"),
        "{report:?}"
    );
    assert!(
        !report
            .files
            .iter()
            .any(|file| file.path == "config.local.json"),
        "ignored file leaked: {report:?}"
    );
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "requires a live FSEvents service; run as a focused native gate"
)]
fn bash_changed_files_directory_rename_never_claims_a_deleted_directory_file() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("src")).unwrap();
    for name in ["one.txt", "two.txt"] {
        std::fs::write(root.path().join("src").join(name), name).unwrap();
    }
    let native = NativeWatcher::new(root.path(), false);
    native.ready();
    let capture = native.capture();
    std::fs::rename(root.path().join("src"), root.path().join("renamed")).unwrap();
    let report = capture.finish();
    assert!(
        !report.files.iter().any(|file| file.path == "src"),
        "directory reported as a file: {report:?}"
    );
    if report.incomplete.is_empty() {
        for name in [
            "src/one.txt",
            "src/two.txt",
            "renamed/one.txt",
            "renamed/two.txt",
        ] {
            assert!(
                report
                    .files
                    .iter()
                    .any(|file| file.path.replace('\\', "/") == name),
                "missing renamed child {name}: {report:?}"
            );
        }
    } else {
        assert!(
            report
                .incomplete
                .iter()
                .any(|reason| reason == NOT_CAUGHT_UP),
            "{report:?}"
        );
    }
}

#[test]
fn bash_changed_files_directory_kind_is_retained_after_rename() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("src")).unwrap();
    let (journal, capture) = capture(root.path(), BashChangedFiles::List);
    event(
        &journal,
        root.path(),
        "src",
        notify::EventKind::Modify(ModifyKind::Name(notify::event::RenameMode::Any)),
    );
    std::fs::rename(root.path().join("src"), root.path().join("renamed")).unwrap();
    let report = capture.finish();
    assert!(
        report.files.is_empty(),
        "directory type must not be re-inferred from a vanished path: {report:?}"
    );
    assert_eq!(report.incomplete, vec![NOT_CAUGHT_UP]);
    assert!(report.changed_files_list_envelope.total.is_at_least());
}

#[cfg(windows)]
#[test]
fn bash_changed_files_windows_canonical_git_root_keeps_before_images() {
    let root = repo();
    for project in [
        root.path().to_path_buf(),
        root.path().canonicalize().unwrap(),
    ] {
        git(root.path(), &["checkout", "--", "clean.txt"]);
        let (journal, capture) = capture(&project, BashChangedFiles::Diff);
        assert!(
            capture.git.is_some(),
            "Git snapshot unavailable: root={project:?}"
        );
        std::fs::write(root.path().join("clean.txt"), "new\n").unwrap();
        std::fs::create_dir_all(root.path().join("nested")).unwrap();
        std::fs::write(root.path().join("nested/new.txt"), "added\n").unwrap();
        event(
            &journal,
            &project,
            "clean.txt",
            notify::EventKind::Modify(ModifyKind::Any),
        );
        event(
            &journal,
            &project,
            "nested/new.txt",
            notify::EventKind::Create(CreateKind::File),
        );
        let report = capture.finish();
        assert!(
            report.files.iter().any(|file| file.path == "clean.txt"
                && file
                    .diff
                    .as_ref()
                    .is_some_and(|diff| diff.contains("-old\n+new"))),
            "Windows before-image lost: {report:?}; git={:?}",
            capture.git
        );
        assert!(
            report.files.iter().any(|file| file.path == "nested/new.txt"
                && file
                    .diff
                    .as_ref()
                    .is_some_and(|diff| diff.contains("+++ b/nested/new.txt"))),
            "Windows added path lost: {report:?}"
        );
        std::fs::remove_dir_all(root.path().join("nested")).unwrap();
    }
}

#[cfg(unix)]
#[test]
fn bash_changed_files_diff_never_reads_swapped_ancestor_git_hook() {
    let root = repo();
    std::fs::write(
        root.path().join(".git/hooks/pre-commit"),
        "PRIVATE_HOOK_MUST_NOT_APPEAR\n",
    )
    .unwrap();
    let (journal, capture) = capture(root.path(), BashChangedFiles::Diff);
    assert!(capture.git.is_some());
    std::fs::create_dir(root.path().join("alias")).unwrap();
    std::fs::write(root.path().join("alias/pre-commit"), "temporary\n").unwrap();
    event(
        &journal,
        root.path(),
        "alias/pre-commit",
        notify::EventKind::Create(CreateKind::File),
    );
    std::fs::remove_dir_all(root.path().join("alias")).unwrap();
    std::os::unix::fs::symlink(root.path().join(".git/hooks"), root.path().join("alias")).unwrap();
    let report = capture.finish();
    assert!(
        !serde_json::to_string(report)
            .unwrap()
            .contains("PRIVATE_HOOK_MUST_NOT_APPEAR"),
        "denied contents leaked: {report:?}"
    );
    let file = report
        .files
        .iter()
        .find(|file| file.path == "alias/pre-commit")
        .expect("path remains listed");
    assert!(file.diff.is_none(), "unsafe read produced a diff: {file:?}");
    assert_eq!(file.reason.as_deref(), Some(NOT_SHOWN));
}

#[cfg(unix)]
#[test]
fn bash_changed_files_diff_rechecks_opened_handle_after_ancestor_swap() {
    let root = repo();
    std::fs::write(
        root.path().join(".git/hooks/pre-commit"),
        "PRIVATE_HOOK_RACE_MUST_NOT_APPEAR\n",
    )
    .unwrap();
    let (journal, capture) = capture(root.path(), BashChangedFiles::Diff);
    std::fs::create_dir(root.path().join("alias")).unwrap();
    std::fs::write(root.path().join("alias/pre-commit"), "temporary\n").unwrap();
    event(
        &journal,
        root.path(),
        "alias/pre-commit",
        notify::EventKind::Create(CreateKind::File),
    );
    let alias = root.path().join("alias");
    let hooks = root.path().join(".git/hooks");
    READ_AFTER_CHECK_FOR_TEST.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            std::fs::remove_dir_all(&alias).unwrap();
            std::os::unix::fs::symlink(hooks, alias).unwrap();
        }))
    });
    let report = capture.finish();
    assert!(!serde_json::to_string(report)
        .unwrap()
        .contains("PRIVATE_HOOK_RACE_MUST_NOT_APPEAR"));
    assert_eq!(report.files[0].reason.as_deref(), Some(NOT_SHOWN));
    assert!(report.files[0].diff.is_none());
}

#[test]
fn bash_changed_files_diff_respects_configured_and_credential_read_floors() {
    let root = repo();
    let home = root.path().join("home");
    std::fs::create_dir_all(home.join(".ssh")).unwrap();
    let secret = root.path().join("private.txt");
    let (journal, mut capture) = capture(root.path(), BashChangedFiles::Diff);
    let mut config = crate::config::Config::default();
    config.sandbox.read_deny = vec![secret];
    let access = crate::access_policy::AccessPolicy::from_config(
        &config,
        &root.path().join(".test-storage"),
        vec![root.path().canonicalize().unwrap()],
        &home,
    );
    Arc::get_mut(&mut capture).unwrap().read_policy = Some(ReadPolicy::new(access, None));
    std::fs::write(
        root.path().join("private.txt"),
        "CONFIGURED_SECRET_MUST_NOT_APPEAR\n",
    )
    .unwrap();
    std::fs::write(
        home.join(".ssh/id_rsa"),
        "CREDENTIAL_SECRET_MUST_NOT_APPEAR\n",
    )
    .unwrap();
    for name in ["private.txt", "home/.ssh/id_rsa"] {
        event(
            &journal,
            root.path(),
            name,
            notify::EventKind::Modify(ModifyKind::Any),
        );
    }
    let report = capture.finish();
    assert_eq!(report.files.len(), 2);
    assert!(
        report
            .files
            .iter()
            .all(|file| file.diff.is_none() && file.reason.as_deref() == Some(NOT_SHOWN)),
        "{report:?}"
    );
    assert!(!serde_json::to_string(report)
        .unwrap()
        .contains("SECRET_MUST_NOT_APPEAR"));
}

#[test]
fn bash_changed_files_diff_inherits_shell_launch_read_denies() {
    let root = repo();
    let storage = tempfile::tempdir().unwrap();
    let secret = root.path().join("environment-secret.txt");
    std::fs::write(&secret, "LAUNCH_PROFILE_SECRET_MUST_NOT_APPEAR\n").unwrap();
    let mut config = crate::config::Config::default();
    config.project_root = Some(root.path().into());
    config.storage_dir = Some(storage.path().into());
    config.bash.changed_files = BashChangedFiles::Diff;
    let ctx =
        crate::context::AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config);
    let mut profile = crate::sandbox_profile::SandboxProfile::build(
        vec![root.path().into()],
        vec![],
        vec![],
        vec![],
        vec![secret.clone()],
        vec![],
        vec![],
        root.path().into(),
    )
    .unwrap();
    profile.deny_prefixes = vec![root.path().join("credential-prefix")];
    let profile = profile.canonicalize_for_launch().unwrap();
    let plan = crate::sandbox_spawn::SpawnPlan::Launcher {
        profile,
        launcher_path: PathBuf::from("unused-test-launcher"),
    };
    let capture = Capture::start_for_spawn(&ctx, "read-floor-test", &plan).unwrap();
    std::fs::write(
        root.path().join("credential-prefix.toml"),
        "PREFIX_SECRET_MUST_NOT_APPEAR\n",
    )
    .unwrap();
    for name in ["environment-secret.txt", "credential-prefix.toml"] {
        event(
            &capture.journal,
            &capture.root,
            name,
            notify::EventKind::Create(CreateKind::File),
        );
    }
    let report = capture.finish();
    assert_eq!(report.files.len(), 2);
    assert!(
        report
            .files
            .iter()
            .all(|file| file.diff.is_none() && file.reason.as_deref() == Some(NOT_SHOWN)),
        "{report:?}"
    );
    assert!(!serde_json::to_string(report)
        .unwrap()
        .contains("SECRET_MUST_NOT_APPEAR"));
}

#[cfg(windows)]
#[test]
fn bash_changed_files_windows_read_policy_compares_path_spellings_without_rewriting() {
    let root = tempfile::tempdir().unwrap();
    let canonical = root.path().canonicalize().unwrap();
    let ordinary = crate::windows_path::normalize_windows_path(&canonical);
    let access = crate::access_policy::AccessPolicy::from_config(
        &crate::config::Config::default(),
        &ordinary.join("storage"),
        vec![ordinary.clone()],
        &ordinary.join("home"),
    );
    let mut policy = ReadPolicy::new(access, None);
    for (stored, target) in [(&canonical, &ordinary), (&ordinary, &canonical)] {
        let prefix = stored.join("credential-prefix");
        policy.deny_prefixes = vec![prefix.clone()];
        assert!(
            !policy.allows(&target.join("credential-prefix.toml")),
            "filename prefix deny missed: stored={stored:?}, target={target:?}"
        );
        assert_eq!(policy.deny_prefixes, vec![prefix]);
        policy.deny_prefixes.clear();
        let data = crate::sandbox_profile::SandboxDataPolicy {
            deny: vec![stored.join("private")],
            read_allow: vec![stored.join("private/readable")],
            ..Default::default()
        };
        policy.native_data = Some(data.clone());
        assert!(!policy.allows(&target.join("private/secret.txt")));
        assert!(policy.allows(&target.join("private/readable/report.txt")));
        assert_eq!(policy.native_data, Some(data));
        policy.native_data = None;
    }
}

#[test]
fn bash_changed_files_large_diff_never_displaces_later_paths() {
    let root = repo();
    let (journal, capture) = capture(root.path(), BashChangedFiles::Diff);
    assert!(capture.git.is_some());
    let huge = (0..300)
        .map(|i| format!("line-{i:03}:{}\n", "x".repeat(220)))
        .collect::<String>();
    for (name, contents) in [
        ("a-huge.txt", huge.as_str()),
        ("b-small.txt", "b\n"),
        ("c-small.txt", "c\n"),
        ("d-small.txt", "d\n"),
    ] {
        std::fs::write(root.path().join(name), contents).unwrap();
        event(
            &journal,
            root.path(),
            name,
            notify::EventKind::Create(CreateKind::File),
        );
    }
    let report = capture.finish();
    assert_eq!(
        report
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>(),
        vec!["a-huge.txt", "b-small.txt", "c-small.txt", "d-small.txt"],
        "diff consumed path space: {report:?}"
    );
    assert!(serde_json::to_vec(report).unwrap().len() <= MAX_REPORT_BYTES);
    assert!(!report.text.contains("no file changes observed"));
}
