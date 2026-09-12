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

use crate::watcher_filter::{derive_excluded_subtrees, SharedGitignore, FSEVENTS_EXCLUSION_LIMIT};

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
        let exclusions = derive_excluded_subtrees(&root, &matcher, Some(FSEVENTS_EXCLUSION_LIMIT));
        super::log_exclusions(&root, &exclusions);

        let (backend_tx, backend_rx) = mpsc::channel();
        let stream = FsEventsStream::start(&root, &exclusions, backend_tx.clone())?;
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
                let mut observed_generation = matcher_generation.load(Ordering::Acquire);

                while !thread_shutdown.load(Ordering::Acquire) {
                    let generation = matcher_generation.load(Ordering::Acquire);
                    if generation != observed_generation {
                        let replacement_exclusions = derive_excluded_subtrees(
                            &root,
                            &matcher,
                            Some(FSEVENTS_EXCLUSION_LIMIT),
                        );
                        match FsEventsStream::start(&root, &replacement_exclusions, stream.sender())
                        {
                            Ok(replacement) => {
                                stream = replacement;
                                observed_generation = generation;
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
        let exclusion_paths = create_cf_path_array(exclusions)?;
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
                cf::CFRelease(exclusion_paths);
                drop(Box::from_raw(context_info));
            }
            return Err(notify::Error::generic("FSEventStreamCreate returned null"));
        }

        let exclusions_set =
            unsafe { fs::FSEventStreamSetExclusionPaths(stream, exclusion_paths) != 0 };
        unsafe {
            cf::CFRelease(exclusion_paths);
        }
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
    use std::sync::RwLock;
    use std::time::{Duration, Instant};

    use ignore::gitignore::GitignoreBuilder;

    use super::*;
    use crate::watcher_filter::{filter_watcher_raw_paths_for_test, WatcherFilterConfig};

    #[test]
    fn fsevents_drop_flags_preserve_typed_rescan_reason() {
        let path = Path::new("/tmp/root");
        let user = translate_event(
            fs::kFSEventStreamEventFlagMustScanSubDirs
                | fs::kFSEventStreamEventFlagUserDropped,
            path,
        );
        let kernel = translate_event(
            fs::kFSEventStreamEventFlagMustScanSubDirs
                | fs::kFSEventStreamEventFlagKernelDropped,
            path,
        );

        assert!(user[0].need_rescan());
        assert_eq!(user[0].info(), Some("rescan: user dropped"));
        assert!(kernel[0].need_rescan());
        assert_eq!(kernel[0].info(), Some("rescan: kernel dropped"));
    }

    #[test]
    #[ignore = "requires a live macOS FSEvents service"]
    fn fsevents_exclusions_drop_ignored_file_flood() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("target")).unwrap();
        std::fs::create_dir(root.path().join("src")).unwrap();
        std::fs::write(root.path().join(".gitignore"), "target/\n").unwrap();
        let canonical_root = std::fs::canonicalize(root.path()).unwrap();
        let mut builder = GitignoreBuilder::new(&canonical_root);
        builder.add(root.path().join(".gitignore"));
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

        for index in 0..5_000 {
            std::fs::write(
                root.path().join("target").join(format!("ignored-{index}")),
                b"ignored",
            )
            .unwrap();
        }
        let expected = (0..3)
            .map(|index| canonical_root.join("src").join(format!("kept-{index}")))
            .collect::<BTreeSet<_>>();
        for path in &expected {
            std::fs::write(path, b"kept").unwrap();
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
        let raw_paths = events.into_iter().flat_map(|event| event.paths);
        let filtered = filter_watcher_raw_paths_for_test(
            &WatcherFilterConfig::new(canonical_root, None),
            &matcher,
            raw_paths,
        );
        assert!(
            raw_event_count < 100,
            "excluded flood delivered {raw_event_count} raw events"
        );
        assert_eq!(filtered.changed, expected);
    }
}
