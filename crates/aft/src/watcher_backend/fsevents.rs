use std::ffi::{CStr, OsString};
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use fsevent_sys as fs;
use fsevent_sys::core_foundation as cf;
use notify::event::{
    CreateKind, DataChange, Flag, MetadataKind, ModifyKind, RemoveKind, RenameMode,
};
use notify::{Event, EventKind, RecursiveMode, Watcher};

use crate::watcher_filter::{
    derive_watcher_exclusion_plan, watcher_exclusion_paths, SharedGitignore,
    WATCHER_EXCLUSION_LIMIT,
};

const FSEVENTS_LATENCY_SECONDS: f64 = 0.03;
const BACKEND_POLL_INTERVAL: Duration = Duration::from_millis(50);

pub(crate) struct ProjectWatcher {
    shutdown: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl ProjectWatcher {
    pub(crate) fn create(
        root: PathBuf,
        extra_watch_paths: Vec<PathBuf>,
        tx: mpsc::Sender<notify::Result<Event>>,
        matcher: SharedGitignore,
        matcher_generation: Arc<AtomicU64>,
    ) -> notify::Result<Self> {
        let root = std::fs::canonicalize(&root).unwrap_or(root);
        // Same rule as the inotify backend: the exclusion set describes the
        // matcher at this generation, so the generation is captured beside it
        // rather than on the backend thread after spawn.
        let observed_generation = matcher_generation.load(Ordering::Acquire);
        let plan = derive_watcher_exclusion_plan(&root, &matcher, Some(WATCHER_EXCLUSION_LIMIT));
        let exclusions = plan.selected;
        let exclusion_paths = watcher_exclusion_paths(&exclusions);
        super::log_exclusions(&root, &exclusions, observed_generation);

        let (backend_tx, backend_rx) = mpsc::channel();
        let stream = FsEventsStream::start(&root, &exclusion_paths, backend_tx.clone())?;
        let counters = crate::context::watcher_counters_for_root(&root);
        counters.set_backend_exclusions(
            observed_generation,
            exclusion_paths,
            watcher_exclusion_paths(&plan.dropped),
        );
        let mut external_watcher = notify::recommended_watcher(backend_tx)?;
        for path in extra_watch_paths {
            if path.exists() {
                external_watcher.watch(&path, RecursiveMode::NonRecursive)?;
            }
        }

        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_shutdown = Arc::clone(&shutdown);
        let join = thread::Builder::new()
            .name("aft-fsevents-backend".to_string())
            .spawn(move || {
                let mut stream = stream;
                let _external_watcher = external_watcher;
                let mut exclusions = exclusions;
                let mut observed_generation = observed_generation;

                while !thread_shutdown.load(Ordering::Acquire) {
                    let generation = matcher_generation.load(Ordering::Acquire);
                    if generation != observed_generation {
                        let replacement_plan = derive_watcher_exclusion_plan(
                            &root,
                            &matcher,
                            Some(WATCHER_EXCLUSION_LIMIT),
                        );
                        let replacement_exclusions = replacement_plan.selected;
                        let replacement_paths = watcher_exclusion_paths(&replacement_exclusions);
                        match FsEventsStream::start(&root, &replacement_paths, stream.sender()) {
                            Ok(replacement) => {
                                stream = replacement;
                                observed_generation = generation;
                                counters.set_backend_exclusions(
                                    observed_generation,
                                    replacement_paths,
                                    watcher_exclusion_paths(&replacement_plan.dropped),
                                );
                                if replacement_exclusions != exclusions {
                                    super::log_exclusions(
                                        &root,
                                        &replacement_exclusions,
                                        observed_generation,
                                    );
                                    exclusions = replacement_exclusions;
                                }
                            }
                            Err(error) => {
                                let _ = tx.send(Err(error));
                                return;
                            }
                        }
                    }

                    match backend_rx.recv_timeout(BACKEND_POLL_INTERVAL) {
                        Ok(event) => {
                            if tx.send(event).is_err() {
                                return;
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => return,
                    }
                }
            })
            .map_err(notify::Error::io)?;

        Ok(Self {
            shutdown,
            join: Some(join),
        })
    }
}

impl Drop for ProjectWatcher {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

struct FsEventsStream {
    run_loop: usize,
    join: Option<JoinHandle<()>>,
    sender: mpsc::Sender<notify::Result<Event>>,
}

impl FsEventsStream {
    fn start(
        root: &Path,
        exclusions: &[PathBuf],
        sender: mpsc::Sender<notify::Result<Event>>,
    ) -> notify::Result<Self> {
        let watched_paths = create_cf_path_array(std::slice::from_ref(&root.to_path_buf()))?;
        let context_info = Box::into_raw(Box::new(CallbackContext {
            sender: sender.clone(),
        }));
        let context = fs::FSEventStreamContext {
            version: 0,
            info: context_info.cast(),
            retain: None,
            release: Some(release_context),
            copy_description: None,
        };
        let stream = unsafe {
            fs::FSEventStreamCreate(
                cf::kCFAllocatorDefault,
                callback,
                &context,
                watched_paths,
                fs::kFSEventStreamEventIdSinceNow,
                FSEVENTS_LATENCY_SECONDS,
                fs::kFSEventStreamCreateFlagFileEvents,
            )
        };
        unsafe {
            cf::CFRelease(watched_paths);
        }
        if stream.is_null() {
            unsafe {
                drop(Box::from_raw(context_info));
            }
            return Err(notify::Error::generic("FSEventStreamCreate returned null"));
        }

        let exclusions_set = if exclusions.is_empty() {
            true
        } else {
            let exclusion_paths = match create_cf_path_array(exclusions) {
                Ok(paths) => paths,
                Err(error) => {
                    unsafe {
                        fs::FSEventStreamInvalidate(stream);
                        fs::FSEventStreamRelease(stream);
                    }
                    return Err(error);
                }
            };
            let accepted =
                unsafe { fs::FSEventStreamSetExclusionPaths(stream, exclusion_paths) != 0 };
            unsafe {
                cf::CFRelease(exclusion_paths);
            }
            accepted
        };

        if !exclusions_set {
            unsafe {
                fs::FSEventStreamInvalidate(stream);
                fs::FSEventStreamRelease(stream);
            }
            return Err(notify::Error::generic(
                "FSEventStreamSetExclusionPaths rejected the exclusion list",
            ));
        }

        let stream_address = stream as usize;
        let (run_loop_tx, run_loop_rx) = mpsc::sync_channel(1);
        let join = match thread::Builder::new()
            .name("aft-fsevents-runloop".to_string())
            .spawn(move || {
                let stream = stream_address as fs::FSEventStreamRef;
                unsafe {
                    let run_loop = cf::CFRunLoopGetCurrent();
                    fs::FSEventStreamScheduleWithRunLoop(
                        stream,
                        run_loop,
                        cf::kCFRunLoopDefaultMode,
                    );
                    if fs::FSEventStreamStart(stream) == 0 {
                        let _ = run_loop_tx.send(0);
                    } else {
                        let _ = run_loop_tx.send(run_loop as usize);
                        cf::CFRunLoopRun();
                    }
                    fs::FSEventStreamStop(stream);
                    fs::FSEventStreamInvalidate(stream);
                    fs::FSEventStreamRelease(stream);
                }
            }) {
            Ok(join) => join,
            Err(error) => {
                unsafe {
                    fs::FSEventStreamInvalidate(stream);
                    fs::FSEventStreamRelease(stream);
                }
                return Err(notify::Error::io(error));
            }
        };
        let run_loop = run_loop_rx.recv().map_err(notify::Error::from)?;
        if run_loop == 0 {
            let _ = join.join();
            return Err(notify::Error::generic("FSEventStreamStart failed"));
        }

        Ok(Self {
            run_loop,
            join: Some(join),
            sender,
        })
    }

    fn sender(&self) -> mpsc::Sender<notify::Result<Event>> {
        self.sender.clone()
    }
}

impl Drop for FsEventsStream {
    fn drop(&mut self) {
        let run_loop = self.run_loop as cf::CFRunLoopRef;
        unsafe {
            while CFRunLoopIsWaiting(run_loop) == 0 {
                thread::yield_now();
            }
            cf::CFRunLoopStop(run_loop);
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

struct CallbackContext {
    sender: mpsc::Sender<notify::Result<Event>>,
}

extern "C" fn release_context(info: *const libc::c_void) {
    unsafe {
        drop(Box::from_raw(info as *mut CallbackContext));
    }
}

extern "C" fn callback(
    _stream: fs::FSEventStreamRef,
    info: *mut libc::c_void,
    event_count: usize,
    event_paths: *mut libc::c_void,
    event_flags: *const fs::FSEventStreamEventFlags,
    _event_ids: *const fs::FSEventStreamEventId,
) {
    unsafe {
        let context = &*(info as *const CallbackContext);
        let paths = event_paths as *const *const libc::c_char;
        for index in 0..event_count {
            let bytes = CStr::from_ptr(*paths.add(index)).to_bytes().to_vec();
            let path = PathBuf::from(OsString::from_vec(bytes));
            let flags = *event_flags.add(index);
            for event in translate_event(flags, &path) {
                if context.sender.send(Ok(event)).is_err() {
                    return;
                }
            }
        }
    }
}

fn translate_event(flags: fs::FSEventStreamEventFlags, path: &Path) -> Vec<Event> {
    if has_flag(flags, fs::kFSEventStreamEventFlagHistoryDone) {
        return Vec::new();
    }

    let mut events = Vec::new();
    if has_flag(flags, fs::kFSEventStreamEventFlagMustScanSubDirs) {
        let event = Event::new(EventKind::Other).set_flag(Flag::Rescan);
        events.push(if has_flag(flags, fs::kFSEventStreamEventFlagUserDropped) {
            event.set_info("rescan: user dropped")
        } else if has_flag(flags, fs::kFSEventStreamEventFlagKernelDropped) {
            event.set_info("rescan: kernel dropped")
        } else {
            event
        });
    }
    if has_flag(flags, fs::kFSEventStreamEventFlagRootChanged) {
        events.push(
            Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::From)))
                .set_info("root changed"),
        );
    }
    if has_flag(flags, fs::kFSEventStreamEventFlagMount) {
        events.push(Event::new(EventKind::Create(CreateKind::Other)).set_info("mount"));
    }
    if has_flag(flags, fs::kFSEventStreamEventFlagUnmount) {
        events.push(Event::new(EventKind::Remove(RemoveKind::Other)).set_info("unmount"));
    }
    if has_flag(flags, fs::kFSEventStreamEventFlagItemCreated) {
        events.push(Event::new(EventKind::Create(item_create_kind(flags))));
    }
    if has_flag(flags, fs::kFSEventStreamEventFlagItemRemoved) {
        events.push(Event::new(EventKind::Remove(item_remove_kind(flags))));
    }
    if has_flag(flags, fs::kFSEventStreamEventFlagItemRenamed) {
        events.push(Event::new(EventKind::Modify(ModifyKind::Name(
            RenameMode::Any,
        ))));
    }
    if has_flag(flags, fs::kFSEventStreamEventFlagItemInodeMetaMod) {
        events.push(Event::new(EventKind::Modify(ModifyKind::Metadata(
            MetadataKind::Any,
        ))));
    }
    if has_flag(flags, fs::kFSEventStreamEventFlagItemFinderInfoMod) {
        events.push(
            Event::new(EventKind::Modify(ModifyKind::Metadata(MetadataKind::Other)))
                .set_info("meta: finder info"),
        );
    }
    if has_flag(flags, fs::kFSEventStreamEventFlagItemChangeOwner) {
        events.push(Event::new(EventKind::Modify(ModifyKind::Metadata(
            MetadataKind::Ownership,
        ))));
    }
    if has_flag(flags, fs::kFSEventStreamEventFlagItemXattrMod) {
        events.push(Event::new(EventKind::Modify(ModifyKind::Metadata(
            MetadataKind::Extended,
        ))));
    }
    if has_flag(flags, fs::kFSEventStreamEventFlagItemModified) {
        events.push(Event::new(EventKind::Modify(ModifyKind::Data(
            DataChange::Content,
        ))));
    }
    if events.is_empty() {
        events.push(Event::new(EventKind::Any));
    }

    events
        .into_iter()
        .map(|event| event.add_path(path.to_path_buf()))
        .collect()
}

fn item_create_kind(flags: fs::FSEventStreamEventFlags) -> CreateKind {
    if has_flag(flags, fs::kFSEventStreamEventFlagItemIsDir) {
        CreateKind::Folder
    } else if has_flag(flags, fs::kFSEventStreamEventFlagItemIsFile) {
        CreateKind::File
    } else {
        CreateKind::Any
    }
}

fn item_remove_kind(flags: fs::FSEventStreamEventFlags) -> RemoveKind {
    if has_flag(flags, fs::kFSEventStreamEventFlagItemIsDir) {
        RemoveKind::Folder
    } else if has_flag(flags, fs::kFSEventStreamEventFlagItemIsFile) {
        RemoveKind::File
    } else {
        RemoveKind::Any
    }
}

fn has_flag(flags: fs::FSEventStreamEventFlags, flag: fs::FSEventStreamEventFlags) -> bool {
    flags & flag != 0
}

fn create_cf_path_array(paths: &[PathBuf]) -> notify::Result<cf::CFMutableArrayRef> {
    let array =
        unsafe { cf::CFArrayCreateMutable(cf::kCFAllocatorDefault, 0, &cf::kCFTypeArrayCallBacks) };
    if array.is_null() {
        return Err(notify::Error::generic("CFArrayCreateMutable returned null"));
    }

    for path in paths {
        let Some(path) = path.to_str() else {
            unsafe {
                cf::CFRelease(array);
            }
            return Err(notify::Error::generic("FSEvents paths must be valid UTF-8"));
        };
        let mut error = ptr::null_mut();
        let value = unsafe { cf::str_path_to_cfstring_ref(path, &mut error) };
        if value.is_null() {
            unsafe {
                if !error.is_null() {
                    cf::CFRelease(error.cast());
                }
                cf::CFRelease(array);
            }
            return Err(notify::Error::generic("failed to create FSEvents path"));
        }
        unsafe {
            cf::CFArrayAppendValue(array, value);
            cf::CFRelease(value);
        }
    }
    Ok(array)
}

extern "C" {
    fn CFRunLoopIsWaiting(run_loop: cf::CFRunLoopRef) -> cf::Boolean;
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::process::Command;
    use std::sync::RwLock;
    use std::time::{Duration, Instant};

    use ignore::gitignore::GitignoreBuilder;

    use super::*;
    use crate::watcher_filter::{
        derive_excluded_subtrees, derive_watcher_exclusion_plan, filter_watcher_raw_paths_for_test,
        run_watcher_thread, watcher_dispatch_channel, WatcherDispatchEvent, WatcherFilterConfig,
    };

    fn run_git(directory: &Path, args: &[&str]) {
        assert!(
            Command::new("git")
                .current_dir(directory)
                .args(args)
                .status()
                .expect("run git")
                .success(),
            "git {args:?} failed in {}",
            directory.display()
        );
    }

    /// A real linked worktree shaped like a TypeScript monorepo: `.git` is a
    /// file, the only heavy ignored directory is `packages/plugin/node_modules`,
    /// and the ignore file lists eight boundaries that exist ahead of it. The
    /// root names a Rust repository would spend slots on (`target`, `build`,
    /// `.cache`, `coverage`) never exist here.
    fn specimen_typescript_worktree() -> (tempfile::TempDir, PathBuf) {
        let container = tempfile::tempdir().unwrap();
        let main = container.path().join("main");
        std::fs::create_dir(&main).unwrap();
        run_git(&main, &["init"]);
        std::fs::write(main.join("package.json"), "{}\n").unwrap();
        let contested = (0..8)
            .map(|index| format!("logs-{index}/\n"))
            .collect::<String>();
        std::fs::write(
            main.join(".gitignore"),
            format!(
                "{contested}node_modules\ndist\npackages/plugin/dist\npackages/pi-plugin/dist\n"
            ),
        )
        .unwrap();
        run_git(&main, &["add", "."]);
        run_git(
            &main,
            &[
                "-c",
                "user.name=AFT Test",
                "-c",
                "user.email=aft@example.invalid",
                "commit",
                "-m",
                "fixture",
            ],
        );
        let worktree = container.path().join("pool");
        run_git(
            &main,
            &[
                "worktree",
                "add",
                "-b",
                "watcher-specimen",
                worktree.to_str().unwrap(),
            ],
        );

        for index in 0..8 {
            std::fs::create_dir(worktree.join(format!("logs-{index}"))).unwrap();
        }
        for existing in [
            "packages/plugin/node_modules",
            "packages/plugin/src",
            "packages/plugin/dist",
            "packages/pi-plugin/dist",
        ] {
            std::fs::create_dir_all(worktree.join(existing)).unwrap();
        }
        assert!(worktree.join(".git").is_file(), "a linked worktree");
        for absent in ["target", "build", "dist", ".cache", "coverage"] {
            assert!(!worktree.join(absent).exists());
        }
        let worktree = std::fs::canonicalize(worktree).unwrap();
        (container, worktree)
    }

    fn count_events_under(
        rx: &mpsc::Receiver<notify::Result<Event>>,
        prefix: &Path,
    ) -> notify::Result<usize> {
        let mut count = 0;
        for event in rx.try_iter() {
            let event = event?;
            if event.paths.iter().any(|path| path.starts_with(prefix)) {
                count += 1;
            }
        }
        Ok(count)
    }

    struct LinkedWorktree {
        repository_root: PathBuf,
        path: PathBuf,
        _container: tempfile::TempDir,
    }

    impl Drop for LinkedWorktree {
        fn drop(&mut self) {
            let _ = Command::new("git")
                .current_dir(&self.repository_root)
                .args(["worktree", "remove", "--force"])
                .arg(&self.path)
                .status();
        }
    }

    fn fresh_linked_worktree() -> LinkedWorktree {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let output = Command::new("git")
            .current_dir(manifest_dir)
            .args(["rev-parse", "--show-toplevel"])
            .output()
            .expect("locate repository root");
        assert!(output.status.success(), "git rev-parse failed");
        let repository_root = PathBuf::from(
            String::from_utf8(output.stdout)
                .expect("repository root is UTF-8")
                .trim(),
        );
        let container = tempfile::tempdir().unwrap();
        let path = container.path().join("linked");
        assert!(
            Command::new("git")
                .current_dir(&repository_root)
                .args(["worktree", "add", "--detach"])
                .arg(&path)
                .arg("HEAD")
                .status()
                .expect("create linked worktree")
                .success(),
            "git worktree add failed"
        );
        LinkedWorktree {
            repository_root,
            path,
            _container: container,
        }
    }

    #[test]
    fn fsevents_drop_flags_preserve_typed_rescan_reason() {
        let path = Path::new("/tmp/root");
        let user = translate_event(
            fs::kFSEventStreamEventFlagMustScanSubDirs | fs::kFSEventStreamEventFlagUserDropped,
            path,
        );
        let kernel = translate_event(
            fs::kFSEventStreamEventFlagMustScanSubDirs | fs::kFSEventStreamEventFlagKernelDropped,
            path,
        );

        assert!(user[0].need_rescan());
        assert_eq!(user[0].info(), Some("rescan: user dropped"));
        assert!(kernel[0].need_rescan());
        assert_eq!(kernel[0].info(), Some("rescan: kernel dropped"));
    }

    /// The backend half of a matcher loaded after the watcher started: the
    /// first plan can only see `.git`, and publishing the matcher (which always
    /// bumps the generation) must make the running backend re-derive and
    /// install `target/` without a restart.
    #[test]
    #[ignore = "requires a live macOS FSEvents service"]
    fn backend_rederives_exclusions_when_matcher_is_published_after_start() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(".git")).unwrap();
        std::fs::write(root.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        std::fs::write(root.path().join(".gitignore"), "/target/\n").unwrap();
        std::fs::create_dir_all(root.path().join("target/debug")).unwrap();
        let canonical_root = std::fs::canonicalize(root.path()).unwrap();
        let target = canonical_root.join("target");
        let matcher: SharedGitignore = Arc::new(RwLock::new(None));
        let generation = Arc::new(AtomicU64::new(0));
        let counters = crate::context::watcher_counters_for_root(&canonical_root);
        let (tx, _rx) = mpsc::channel();
        let watcher = ProjectWatcher::create(
            canonical_root.clone(),
            Vec::new(),
            tx,
            Arc::clone(&matcher),
            Arc::clone(&generation),
        )
        .unwrap();
        let before = counters.backend_exclusions();
        assert_eq!(before.matcher_generation, 0);
        assert_eq!(before.paths, vec![canonical_root.join(".git")]);

        let mut builder = GitignoreBuilder::new(&canonical_root);
        builder.add(canonical_root.join(".gitignore"));
        *matcher.write().unwrap() = Some(Arc::new(builder.build().unwrap()));
        generation.fetch_add(1, Ordering::SeqCst);

        let deadline = Instant::now() + Duration::from_secs(2);
        while counters.backend_exclusions().matcher_generation != 1 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let after = counters.backend_exclusions();
        drop(watcher);
        assert_eq!(after.matcher_generation, 1);
        assert!(
            after.paths.contains(&target),
            "published matcher never reached the running backend: {:?}",
            after.paths
        );
    }

    #[test]
    #[ignore = "requires a live macOS FSEvents service"]
    fn fsevents_excludes_directory_created_after_stream_start() {
        let root = tempfile::tempdir().unwrap();
        let canonical_root = std::fs::canonicalize(root.path()).unwrap();
        let excluded = canonical_root.join("node_modules");
        assert!(!excluded.exists());

        let (tx, rx) = mpsc::channel();
        let stream =
            FsEventsStream::start(&canonical_root, std::slice::from_ref(&excluded), tx).unwrap();
        thread::sleep(Duration::from_millis(100));
        let _startup_events = rx.try_iter().collect::<Vec<_>>();

        std::fs::create_dir(&excluded).unwrap();
        for index in 0..200 {
            std::fs::write(
                excluded.join(format!("package-{index}.js")),
                b"export {};\n",
            )
            .unwrap();
        }
        thread::sleep(Duration::from_millis(500));

        let events = rx.try_iter().collect::<Vec<_>>();
        for event in &events {
            if let Err(error) = event {
                panic!("watcher error during exclusion measurement: {error}");
            }
        }
        let delivered_for_excluded_prefix = events
            .iter()
            .filter_map(|event| event.as_ref().ok())
            .filter(|event| event.paths.iter().any(|path| path.starts_with(&excluded)))
            .count();
        let delivered_for_excluded_descendants = events
            .iter()
            .filter_map(|event| event.as_ref().ok())
            .filter(|event| {
                event
                    .paths
                    .iter()
                    .any(|path| path.starts_with(&excluded) && path != &excluded)
            })
            .count();
        eprintln!(
            "absent FSEvents exclusion delivered {delivered_for_excluded_prefix} events for {}",
            excluded.display()
        );
        drop(stream);

        assert_eq!(delivered_for_excluded_prefix, 1);
        assert_eq!(delivered_for_excluded_descendants, 0);
    }

    #[test]
    #[ignore = "requires a live macOS FSEvents service"]
    fn ecosystem_seed_drops_fresh_target_debug_burst_from_raw_stream() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        std::fs::write(root.path().join("package.json"), "{}\n").unwrap();
        std::fs::create_dir(root.path().join("node_modules")).unwrap();
        let nested_node_modules = [
            "packages/plugin/node_modules",
            "packages/pi-plugin/node_modules",
            "packages/cli/node_modules",
            "packages/dashboard/node_modules",
            "packages/e2e-tests/node_modules",
            "packages/docs/node_modules",
            "packages/retina-local-fs/node_modules",
        ];
        for relative in nested_node_modules {
            std::fs::create_dir_all(root.path().join(relative)).unwrap();
        }
        let nested_ignores = nested_node_modules
            .iter()
            .map(|relative| format!("/{relative}/\n"))
            .collect::<String>();
        std::fs::write(
            root.path().join(".gitignore"),
            format!("/node_modules/\n{nested_ignores}/target/\n"),
        )
        .unwrap();
        let canonical_root = std::fs::canonicalize(root.path()).unwrap();
        let target = canonical_root.join("target");
        let mut builder = GitignoreBuilder::new(&canonical_root);
        builder.add(root.path().join(".gitignore"));
        let matcher = Arc::new(RwLock::new(Some(Arc::new(builder.build().unwrap()))));
        let exclusions =
            derive_excluded_subtrees(&canonical_root, &matcher, Some(WATCHER_EXCLUSION_LIMIT));
        let exclusion_paths = watcher_exclusion_paths(&exclusions);
        // Although eight existing `node_modules` paths could consume every
        // exclusion slot, retaining the absent `target` path reserves one for
        // Rust and lets FSEvents apply it if the build creates `target` later.
        assert!(
            exclusion_paths.contains(&target),
            "absent target seed lost its slot: {exclusion_paths:?}"
        );
        assert!(exclusion_paths.contains(&canonical_root.join("node_modules")));
        assert!(!target.exists());

        let (tx, rx) = mpsc::channel();
        let stream = FsEventsStream::start(&canonical_root, &exclusion_paths, tx).unwrap();
        thread::sleep(Duration::from_millis(100));
        let _startup_events = rx.try_iter().collect::<Vec<_>>();
        let debug = target.join("debug");
        std::fs::create_dir_all(&debug).unwrap();
        thread::sleep(Duration::from_millis(500));
        let _boundary_events = rx.try_iter().collect::<Vec<_>>();

        for index in 0..200 {
            std::fs::write(debug.join(format!("artifact-{index}.o")), b"object\n").unwrap();
        }
        thread::sleep(Duration::from_millis(500));
        let events = rx.try_iter().collect::<Vec<_>>();
        drop(stream);

        for event in &events {
            if let Err(error) = event {
                panic!("watcher error during excluded target burst: {error}");
            }
        }
        assert_eq!(
            events.len(),
            0,
            "the excluded target/debug burst must not reach the raw stream"
        );
    }

    #[test]
    #[ignore = "requires a live macOS FSEvents service"]
    fn fresh_root_node_modules_burst_has_no_overflow_or_rescan() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join(".gitignore"), "node_modules/\n").unwrap();
        let canonical_root = std::fs::canonicalize(root.path()).unwrap();
        let excluded = canonical_root.join("node_modules");
        let mut builder = GitignoreBuilder::new(&canonical_root);
        builder.add(root.path().join(".gitignore"));
        let matcher = Arc::new(RwLock::new(Some(Arc::new(builder.build().unwrap()))));
        let generation = Arc::new(AtomicU64::new(1));
        let config = WatcherFilterConfig::new(canonical_root.clone(), None);
        let counters = crate::context::watcher_counters_for_root(&canonical_root);
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_shutdown = Arc::clone(&shutdown);
        let backend_matcher = Arc::clone(&matcher);
        let backend_generation = Arc::clone(&generation);
        let (dispatch_tx, dispatch_rx) = watcher_dispatch_channel();
        let watcher_thread = thread::spawn(move || {
            run_watcher_thread(
                config,
                Vec::new(),
                matcher,
                generation,
                dispatch_tx,
                thread_shutdown,
                move |root, extra_paths, tx| {
                    ProjectWatcher::create(
                        root,
                        extra_paths,
                        tx,
                        backend_matcher,
                        backend_generation,
                    )
                },
            );
        });
        let startup_deadline = Instant::now() + Duration::from_secs(2);
        while counters.backend_exclusions().matcher_generation != 1
            && Instant::now() < startup_deadline
        {
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(counters.backend_exclusions().matcher_generation, 1);

        std::fs::create_dir(&excluded).unwrap();
        for index in 0..2_000 {
            std::fs::write(
                excluded.join(format!("package-{index}.js")),
                b"export {};\n",
            )
            .unwrap();
        }

        let observation_deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < observation_deadline {
            match dispatch_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(WatcherDispatchEvent::RescanRequired(reason)) => {
                    let _ = counters.begin_rescan(reason);
                }
                Ok(
                    WatcherDispatchEvent::Paths(_)
                    | WatcherDispatchEvent::IgnoreRulesChanged { .. }
                    | WatcherDispatchEvent::RootDeleted,
                ) => {}
                Ok(WatcherDispatchEvent::Error(error)) => panic!("watcher error: {error}"),
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            }
        }
        shutdown.store(true, Ordering::SeqCst);
        watcher_thread.join().unwrap();

        let snapshot = counters.snapshot();
        assert_eq!(snapshot.overflows_total, 0);
        assert_eq!(snapshot.rescans_buffer_overflow_total, 0);
        assert_eq!(snapshot.rescans_kernel_dropped_total, 0);
        assert_eq!(snapshot.rescans_user_dropped_total, 0);
        assert_eq!(snapshot.rescans_unknown_total, 0);
        assert!(counters.backend_exclusions().paths.contains(&excluded));
    }

    #[test]
    #[ignore = "requires a live macOS FSEvents service and Cargo"]
    fn fsevents_exclusions_drop_fresh_worktree_cargo_build() {
        let worktree = fresh_linked_worktree();
        let root = &worktree.path;
        let probe = root.join("watcher-exclusion-probe");
        std::fs::create_dir(root.join("target")).unwrap();
        for directory in [
            "node_modules",
            "dist",
            "build",
            ".next",
            ".venv",
            "venv",
            "__pycache__",
            "coverage",
            ".turbo",
        ] {
            std::fs::create_dir(root.join(directory)).unwrap();
        }
        std::fs::create_dir_all(probe.join("src")).unwrap();
        std::fs::write(
            probe.join("Cargo.toml"),
            "[package]\nname = \"watcher-exclusion-probe\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n[workspace]\n",
        )
        .unwrap();
        std::fs::write(
            probe.join("Cargo.lock"),
            "# This file is automatically @generated by Cargo.\nversion = 4\n\n[[package]]\nname = \"watcher-exclusion-probe\"\nversion = \"0.0.0\"\n",
        )
        .unwrap();
        std::fs::write(probe.join("src/main.rs"), "fn main() {}\n").unwrap();

        let canonical_root = std::fs::canonicalize(root).unwrap();
        let mut builder = GitignoreBuilder::new(&canonical_root);
        builder.add(root.join(".gitignore"));
        let matcher = Arc::new(RwLock::new(Some(Arc::new(builder.build().unwrap()))));
        let generation = Arc::new(AtomicU64::new(1));
        let (tx, rx) = mpsc::channel();
        let watcher = ProjectWatcher::create(
            canonical_root.clone(),
            Vec::new(),
            tx,
            Arc::clone(&matcher),
            generation,
        )
        .unwrap();
        thread::sleep(Duration::from_millis(100));
        assert_eq!(
            rx.try_iter().count(),
            0,
            "watcher must start with an empty stream"
        );

        let build = Command::new(env!("CARGO"))
            .current_dir(root)
            .args(["build", "--locked", "--offline", "--manifest-path"])
            .arg(probe.join("Cargo.toml"))
            .arg("--target-dir")
            .arg(root.join("target"))
            .output()
            .expect("run tiny Cargo build");
        assert!(
            build.status.success(),
            "tiny Cargo build failed: {}",
            String::from_utf8_lossy(&build.stderr)
        );

        thread::sleep(Duration::from_millis(500));
        let excluded_raw_events = rx.try_iter().collect::<Vec<_>>();
        for event in &excluded_raw_events {
            if let Err(error) = event {
                panic!("watcher error during excluded build: {error}");
            }
        }
        eprintln!("excluded build raw events: {}", excluded_raw_events.len());
        assert_eq!(
            excluded_raw_events.len(),
            0,
            "the excluded target build must not reach the raw channel"
        );

        let expected = (0..3)
            .map(|index| {
                canonical_root
                    .join("watcher-exclusion-probe/src")
                    .join(format!("kept-{index}.rs"))
            })
            .collect::<BTreeSet<_>>();
        for path in &expected {
            std::fs::write(path, b"pub fn kept() {}\n").unwrap();
        }

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut events = Vec::new();
        let mut last_event = Instant::now();
        while Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(Ok(event)) => {
                    events.push(event);
                    last_event = Instant::now();
                }
                Ok(Err(error)) => panic!("watcher error: {error}"),
                Err(mpsc::RecvTimeoutError::Timeout)
                    if !events.is_empty() && last_event.elapsed() >= Duration::from_millis(300) =>
                {
                    break;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        drop(watcher);

        let raw_event_count = events.len();
        let raw_paths = events
            .into_iter()
            .filter(|event| crate::watcher_filter::watcher_event_invalidates(&event.kind))
            .flat_map(|event| event.paths);
        let filtered = filter_watcher_raw_paths_for_test(
            &WatcherFilterConfig::new(canonical_root, None),
            &matcher,
            raw_paths,
        );
        assert!(
            raw_event_count < 100,
            "control writes delivered {raw_event_count} raw events"
        );
        assert_eq!(filtered.changed, expected);
    }

    /// The install burst that drove a fleet worktree into repeated watcher
    /// overflow, measured live: a `bun install` into
    /// `packages/plugin/node_modules` must deliver nothing, and the control
    /// shows the same churn is delivered when that path holds no slot.
    #[test]
    #[ignore = "requires a live macOS FSEvents service"]
    fn nested_node_modules_install_burst_never_reaches_the_stream() {
        let (_container, root) = specimen_typescript_worktree();
        let nested = root.join("packages/plugin/node_modules");
        let mut builder = GitignoreBuilder::new(&root);
        builder.add(root.join(".gitignore"));
        let matcher = Arc::new(RwLock::new(Some(Arc::new(builder.build().unwrap()))));
        let plan = derive_watcher_exclusion_plan(&root, &matcher, Some(WATCHER_EXCLUSION_LIMIT));
        let exclusion_paths = watcher_exclusion_paths(&plan.selected);

        let (tx, rx) = mpsc::channel();
        let stream = FsEventsStream::start(&root, &exclusion_paths, tx).unwrap();
        thread::sleep(Duration::from_millis(100));
        let _startup_events = rx.try_iter().count();
        for index in 0..200 {
            std::fs::write(nested.join(format!("module-{index}.js")), b"export {};\n").unwrap();
        }
        thread::sleep(Duration::from_millis(500));
        let excluded_events = count_events_under(&rx, &nested).unwrap();
        drop(stream);
        assert_eq!(
            excluded_events, 0,
            "the install burst reached the raw stream; installed exclusions: {exclusion_paths:?}"
        );
        assert!(
            exclusion_paths.contains(&nested),
            "the directory that floods holds no slot: {exclusion_paths:?}"
        );

        // Same churn, same fixture, one slot removed: if this stream were also
        // silent the zero above would be measuring a quiet directory instead of
        // the exclusion.
        let control_exclusions = exclusion_paths
            .into_iter()
            .filter(|path| path != &nested)
            .collect::<Vec<_>>();
        let (tx, rx) = mpsc::channel();
        let stream = FsEventsStream::start(&root, &control_exclusions, tx).unwrap();
        thread::sleep(Duration::from_millis(100));
        let _startup_events = rx.try_iter().count();
        for index in 200..400 {
            std::fs::write(nested.join(format!("module-{index}.js")), b"export {};\n").unwrap();
        }
        thread::sleep(Duration::from_millis(500));
        let control_events = count_events_under(&rx, &nested).unwrap();
        drop(stream);
        assert!(
            control_events > 0,
            "the unexcluded control stream saw no churn at all"
        );
    }
}
