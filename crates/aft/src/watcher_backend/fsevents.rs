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
    derive_excluded_subtrees, watcher_exclusion_paths, SharedGitignore, WATCHER_EXCLUSION_LIMIT,
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
        let exclusions = derive_excluded_subtrees(&root, &matcher, Some(WATCHER_EXCLUSION_LIMIT));
        let exclusion_paths = watcher_exclusion_paths(&exclusions);
        super::log_exclusions(&root, &exclusions);

        let (backend_tx, backend_rx) = mpsc::channel();
        let stream = FsEventsStream::start(&root, &exclusion_paths, backend_tx.clone())?;
        let counters = crate::context::watcher_counters_for_root(&root);
        counters.set_backend_exclusions(observed_generation, exclusion_paths);
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
                        let replacement_exclusions = derive_excluded_subtrees(
                            &root,
                            &matcher,
                            Some(WATCHER_EXCLUSION_LIMIT),
                        );
                        let replacement_paths = watcher_exclusion_paths(&replacement_exclusions);
                        match FsEventsStream::start(&root, &replacement_paths, stream.sender()) {
                            Ok(replacement) => {
                                stream = replacement;
                                observed_generation = generation;
                                counters.set_backend_exclusions(
                                    observed_generation,
                                    replacement_paths,
                                );
                                if replacement_exclusions != exclusions {
                                    super::log_exclusions(&root, &replacement_exclusions);
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
    use crate::watcher_filter::{filter_watcher_raw_paths_for_test, WatcherFilterConfig};

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
}
