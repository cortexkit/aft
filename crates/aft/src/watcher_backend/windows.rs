use std::collections::BTreeSet;
use std::ffi::{c_void, OsStr, OsString};
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use notify::event::{CreateKind, Flag, ModifyKind, RemoveKind, RenameMode};
use notify::{Event, EventKind};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_INVALID_PARAMETER, ERROR_NOTIFY_ENUM_DIR,
    ERROR_OPERATION_ABORTED, HANDLE, INVALID_HANDLE_VALUE, WAIT_TIMEOUT,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ReadDirectoryChangesW, FILE_ACTION_ADDED, FILE_ACTION_MODIFIED,
    FILE_ACTION_REMOVED, FILE_ACTION_RENAMED_NEW_NAME, FILE_ACTION_RENAMED_OLD_NAME,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY,
    FILE_NOTIFY_CHANGE_ATTRIBUTES, FILE_NOTIFY_CHANGE_CREATION, FILE_NOTIFY_CHANGE_DIR_NAME,
    FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SECURITY,
    FILE_NOTIFY_CHANGE_SIZE, FILE_NOTIFY_INFORMATION, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::{
    CancelIoEx, CreateIoCompletionPort, GetQueuedCompletionStatus, PostQueuedCompletionStatus,
    OVERLAPPED,
};

use crate::watcher_filter::SharedGitignore;

// parcel-watcher starts at 1 MiB (`src/windows/WindowsBackend.cc:8-10,
// 70-80` in the snapshot cited by watcher-backends-reference.md). This is the
// larger reference allocation and gives an ignored build burst room while the
// filter thread drains it.
#[cfg(not(test))]
const WINDOWS_BUFFER_SIZE: usize = 1024 * 1024;
// The native overflow test deliberately constrains the kernel buffer.
#[cfg(test)]
const WINDOWS_BUFFER_SIZE: usize = 256;
const WINDOWS_NETWORK_BUFFER_SIZE: usize = 64 * 1024;
const COMPLETION_POLL_INTERVAL_MS: u32 = 50;
const SHUTDOWN_COMPLETION_KEY: usize = usize::MAX;
const NOTIFY_FILTER: u32 = FILE_NOTIFY_CHANGE_FILE_NAME
    | FILE_NOTIFY_CHANGE_DIR_NAME
    | FILE_NOTIFY_CHANGE_ATTRIBUTES
    | FILE_NOTIFY_CHANGE_SIZE
    | FILE_NOTIFY_CHANGE_LAST_WRITE
    | FILE_NOTIFY_CHANGE_CREATION
    | FILE_NOTIFY_CHANGE_SECURITY;

/// Owned I/O completion port handle, closed exactly once when the last owner
/// drops it.
///
/// Both the watcher and its completion thread hold an `Arc` to the port. The
/// completion thread can exit on its own (the event receiver went away, or a
/// read failed) while the watcher is still alive, and the watcher's `Drop`
/// then posts a shutdown packet to the port. If the thread closed the raw
/// handle on exit, that post would target a stale handle value that Windows
/// may already have handed to an unrelated completion port, such as the one
/// backing a tokio runtime's I/O driver. mio reports a packet with a null
/// OVERLAPPED as an event whose token is the completion key, and tokio turns
/// that token into a pointer; with the shutdown key (`usize::MAX`) this is a
/// dereference of address 0xffff_ffff_ffff_ffff and aborts the process.
/// Shared ownership guarantees the handle stays open for as long as anyone
/// can still post to it.
struct CompletionPort {
    handle: HANDLE,
}

// The completion port is a kernel object designed for concurrent use from
// multiple threads; the wrapper only closes it once, on final drop.
unsafe impl Send for CompletionPort {}
unsafe impl Sync for CompletionPort {}

impl CompletionPort {
    fn create() -> notify::Result<Arc<Self>> {
        let handle = unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, ptr::null_mut(), 0, 1) };
        if handle.is_null() {
            Err(last_notify_error())
        } else {
            Ok(Arc::new(Self { handle }))
        }
    }

    fn raw(&self) -> HANDLE {
        self.handle
    }
}

impl Drop for CompletionPort {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.handle);
        }
    }
}

pub(crate) struct ProjectWatcher {
    shutdown: Arc<AtomicBool>,
    completion_port: Arc<CompletionPort>,
    join: Option<JoinHandle<()>>,
}

impl ProjectWatcher {
    pub(crate) fn create(
        root: PathBuf,
        extra_watch_paths: Vec<PathBuf>,
        tx: mpsc::Sender<notify::Result<Event>>,
        _matcher: SharedGitignore,
        matcher_generation: Arc<AtomicU64>,
    ) -> notify::Result<Self> {
        let root = std::fs::canonicalize(&root).unwrap_or(root);
        // The handle setup belongs to the matcher state visible at this point.
        // Capture it before spawning: a bump before the backend's first
        // instruction must be observed as new work, not mistaken for the state
        // used to create the watch.
        let observed_generation = matcher_generation.load(Ordering::Acquire);
        let counters = crate::context::watcher_counters_for_root(&root);
        counters.set_backend_exclusions(observed_generation, Vec::new(), Vec::new());

        let completion_port = CompletionPort::create()?;
        let thread_port = Arc::clone(&completion_port);
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_shutdown = Arc::clone(&shutdown);
        let thread_counters = Arc::clone(&counters);
        let thread_observed_generation = Arc::new(AtomicU64::new(observed_generation));
        let observed_generation_for_thread = Arc::clone(&thread_observed_generation);
        let (start_tx, start_rx) = mpsc::sync_channel(1);

        let join = match thread::Builder::new()
            .name("aft-windows-rdcw".to_string())
            .spawn(move || {
                pause_backend_start_for_test(&thread_shutdown);
                let result = run_completion_loop(
                    thread_port.raw(),
                    root,
                    extra_watch_paths,
                    tx,
                    thread_shutdown,
                    matcher_generation,
                    observed_generation,
                    observed_generation_for_thread,
                    thread_counters,
                    start_tx,
                );
                if let Err(error) = result {
                    crate::slog_warn!("Windows watcher stopped: {error}");
                }
                // Deliberately not closing the port here: the watcher may
                // still post its shutdown packet to it. The handle closes
                // when the last `Arc<CompletionPort>` drops.
                drop(thread_port);
            }) {
            Ok(join) => join,
            Err(error) => return Err(notify::Error::io(error)),
        };

        match start_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                shutdown,
                completion_port,
                join: Some(join),
            }),
            Ok(Err(error)) => {
                let _ = join.join();
                Err(error)
            }
            Err(error) => {
                let _ = join.join();
                Err(notify::Error::generic(&format!(
                    "Windows watcher startup channel disconnected: {error}"
                )))
            }
        }
    }
}

impl Drop for ProjectWatcher {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        // `self.completion_port` keeps the handle open, so this post reaches
        // this watcher's own port even if the completion thread already
        // exited.
        unsafe {
            PostQueuedCompletionStatus(
                self.completion_port.raw(),
                0,
                SHUTDOWN_COMPLETION_KEY,
                ptr::null(),
            );
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

struct DirectoryWatch {
    directory: PathBuf,
    exact_file: Option<PathBuf>,
    recursive: bool,
    handle: HANDLE,
    buffer: Vec<u8>,
    overlapped: Box<OVERLAPPED>,
    pending: bool,
}

// The handle and its pinned buffer are used only by the completion thread.
unsafe impl Send for DirectoryWatch {}

impl DirectoryWatch {
    fn open(
        directory: PathBuf,
        exact_file: Option<PathBuf>,
        recursive: bool,
        completion_port: HANDLE,
        completion_key: usize,
    ) -> notify::Result<Self> {
        let encoded_path = directory
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>();
        let handle = unsafe {
            CreateFileW(
                encoded_path.as_ptr(),
                FILE_LIST_DIRECTORY,
                FILE_SHARE_READ | FILE_SHARE_DELETE | FILE_SHARE_WRITE,
                ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
                ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(last_notify_error().add_path(directory));
        }

        let associated =
            unsafe { CreateIoCompletionPort(handle, completion_port, completion_key, 0) };
        if associated.is_null() {
            let error = last_notify_error().add_path(directory.clone());
            unsafe {
                CloseHandle(handle);
            }
            return Err(error);
        }

        Ok(Self {
            directory,
            exact_file,
            recursive,
            handle,
            buffer: vec![0; WINDOWS_BUFFER_SIZE],
            overlapped: Box::new(OVERLAPPED::default()),
            pending: false,
        })
    }

    fn submit(&mut self) -> notify::Result<()> {
        *self.overlapped = OVERLAPPED::default();
        match self.submit_current_buffer() {
            Ok(()) => {
                self.pending = true;
                Ok(())
            }
            Err(error)
                if error.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32)
                    && self.buffer.len() > WINDOWS_NETWORK_BUFFER_SIZE =>
            {
                // Windows rejects buffers larger than 64 KiB for network
                // handles. parcel-watcher uses the same 64 KiB retry floor.
                self.buffer.resize(WINDOWS_NETWORK_BUFFER_SIZE, 0);
                *self.overlapped = OVERLAPPED::default();
                self.submit_current_buffer().map_err(notify::Error::io)?;
                self.pending = true;
                Ok(())
            }
            Err(error) => Err(notify::Error::io(error).add_path(self.directory.clone())),
        }
    }

    fn submit_current_buffer(&mut self) -> io::Result<()> {
        let accepted = unsafe {
            ReadDirectoryChangesW(
                self.handle,
                self.buffer.as_mut_ptr().cast::<c_void>(),
                self.buffer.len() as u32,
                i32::from(self.recursive),
                NOTIFY_FILTER,
                ptr::null_mut(),
                self.overlapped.as_mut(),
                None,
            )
        };
        if accepted == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

impl Drop for DirectoryWatch {
    fn drop(&mut self) {
        debug_assert!(!self.pending, "dropping a pending directory read");
        unsafe {
            CloseHandle(self.handle);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_completion_loop(
    completion_port: HANDLE,
    root: PathBuf,
    extra_watch_paths: Vec<PathBuf>,
    tx: mpsc::Sender<notify::Result<Event>>,
    shutdown: Arc<AtomicBool>,
    matcher_generation: Arc<AtomicU64>,
    mut observed_generation: u64,
    observed_generation_out: Arc<AtomicU64>,
    counters: Arc<crate::context::WatcherCounters>,
    start_tx: mpsc::SyncSender<notify::Result<()>>,
) -> notify::Result<()> {
    let mut watches = match open_watches(completion_port, &root, extra_watch_paths) {
        Ok(watches) => watches,
        Err(error) => {
            let _ = start_tx.send(Err(error));
            return Ok(());
        }
    };
    for watch in &mut watches {
        if let Err(error) = watch.submit() {
            cancel_and_drain(completion_port, &mut watches);
            let _ = start_tx.send(Err(error));
            return Ok(());
        }
    }
    if start_tx.send(Ok(())).is_err() {
        cancel_and_drain(completion_port, &mut watches);
        return Ok(());
    }

    pause_completion_drain_for_test(&shutdown);
    let measure_drain =
        std::env::var_os("AFT_WINDOWS_WATCHER_DRAIN_METRICS").as_deref() == Some(OsStr::new("1"));
    let mut drain_stats = DrainStats::new(measure_drain);
    let mut loop_result = Ok(());
    while !shutdown.load(Ordering::Acquire) {
        let generation = matcher_generation.load(Ordering::Acquire);
        if generation != observed_generation {
            observed_generation = generation;
            observed_generation_out.store(generation, Ordering::Release);
            counters.set_backend_exclusions(generation, Vec::new(), Vec::new());
        }

        let Some(completion) = wait_for_completion(completion_port)? else {
            continue;
        };
        if completion.key == SHUTDOWN_COMPLETION_KEY && completion.overlapped.is_null() {
            break;
        }
        let Some(watch_index) = completion.key.checked_sub(1) else {
            loop_result = Err(notify::Error::generic(
                "Windows watcher received an invalid completion key",
            ));
            break;
        };
        let Some(watch) = watches.get_mut(watch_index) else {
            loop_result = Err(notify::Error::generic(
                "Windows watcher completion key was outside its watch set",
            ));
            break;
        };
        if !ptr::eq(completion.overlapped, watch.overlapped.as_mut()) {
            loop_result = Err(notify::Error::generic(
                "Windows watcher completion referenced an unknown request",
            ));
            break;
        }
        watch.pending = false;
        let drain_started = Instant::now();

        let buffer_overflow = completion.error == Some(ERROR_NOTIFY_ENUM_DIR)
            || (completion.error.is_none() && completion.bytes_transferred == 0);
        if let Some(error) = completion.error {
            if error == ERROR_OPERATION_ABORTED {
                if !shutdown.load(Ordering::Acquire) {
                    loop_result = Err(os_notify_error(error).add_path(watch.directory.clone()));
                }
                break;
            }
            if !buffer_overflow {
                loop_result = Err(os_notify_error(error).add_path(watch.directory.clone()));
                break;
            }
        }

        let completed_bytes = if buffer_overflow {
            Vec::new()
        } else {
            let byte_count = completion.bytes_transferred as usize;
            if byte_count > watch.buffer.len() {
                loop_result = Err(notify::Error::generic(
                    "ReadDirectoryChangesW returned more bytes than its buffer",
                ));
                break;
            }
            watch.buffer[..byte_count].to_vec()
        };

        // Rearm before translating or filtering the completed batch. Windows
        // cannot exclude target/ at the API, so this bounds the interval in
        // which another ignored burst can overrun the kernel buffer.
        if let Err(error) = watch.submit() {
            loop_result = Err(error);
            break;
        }

        let events = if buffer_overflow {
            vec![buffer_overflow_event(&root)]
        } else {
            match translate_buffer(
                &completed_bytes,
                &watch.directory,
                watch.exact_file.as_deref(),
            ) {
                Ok(events) => events,
                Err(error) => {
                    loop_result = Err(error);
                    break;
                }
            }
        };
        let event_count = events.len();
        for event in events {
            if tx.send(Ok(event)).is_err() {
                loop_result = Ok(());
                shutdown.store(true, Ordering::Release);
                break;
            }
        }
        drain_stats.note(drain_started.elapsed(), event_count, &root);
    }

    cancel_and_drain(completion_port, &mut watches);
    drain_stats.log(&root);
    if let Err(error) = &loop_result {
        let _ = tx.send(Err(notify::Error::generic(&error.to_string())));
    }
    loop_result
}

fn open_watches(
    completion_port: HANDLE,
    root: &Path,
    extra_watch_paths: Vec<PathBuf>,
) -> notify::Result<Vec<DirectoryWatch>> {
    let mut targets = vec![(root.to_path_buf(), None, true)];
    let mut seen = BTreeSet::new();
    seen.insert((root.to_path_buf(), None));

    for path in extra_watch_paths {
        if !path.exists() {
            continue;
        }
        let path = std::fs::canonicalize(&path).unwrap_or(path);
        let (directory, exact_file, recursive) = if path.is_dir() {
            (path, None, false)
        } else {
            let Some(parent) = path.parent() else {
                continue;
            };
            (parent.to_path_buf(), Some(path), false)
        };
        if seen.insert((directory.clone(), exact_file.clone())) {
            targets.push((directory, exact_file, recursive));
        }
    }

    targets
        .into_iter()
        .enumerate()
        .map(|(index, (directory, exact_file, recursive))| {
            DirectoryWatch::open(directory, exact_file, recursive, completion_port, index + 1)
        })
        .collect()
}

struct Completion {
    bytes_transferred: u32,
    key: usize,
    overlapped: *mut OVERLAPPED,
    error: Option<u32>,
}

fn wait_for_completion(completion_port: HANDLE) -> notify::Result<Option<Completion>> {
    let mut bytes_transferred = 0u32;
    let mut key = 0usize;
    let mut overlapped = ptr::null_mut();
    let completed = unsafe {
        GetQueuedCompletionStatus(
            completion_port,
            &mut bytes_transferred,
            &mut key,
            &mut overlapped,
            COMPLETION_POLL_INTERVAL_MS,
        )
    };
    if completed != 0 {
        return Ok(Some(Completion {
            bytes_transferred,
            key,
            overlapped,
            error: None,
        }));
    }

    let error = unsafe { GetLastError() };
    if overlapped.is_null() && error == WAIT_TIMEOUT {
        return Ok(None);
    }
    if overlapped.is_null() {
        return Err(os_notify_error(error));
    }
    Ok(Some(Completion {
        bytes_transferred,
        key,
        overlapped,
        error: Some(error),
    }))
}

fn cancel_and_drain(completion_port: HANDLE, watches: &mut [DirectoryWatch]) {
    let mut remaining = 0usize;
    for watch in watches.iter_mut().filter(|watch| watch.pending) {
        remaining += 1;
        unsafe {
            CancelIoEx(watch.handle, ptr::null());
        }
    }

    while remaining > 0 {
        let mut bytes_transferred = 0u32;
        let mut key = 0usize;
        let mut overlapped = ptr::null_mut();
        unsafe {
            GetQueuedCompletionStatus(
                completion_port,
                &mut bytes_transferred,
                &mut key,
                &mut overlapped,
                u32::MAX,
            );
        }
        if overlapped.is_null() || key == SHUTDOWN_COMPLETION_KEY {
            continue;
        }
        if let Some(watch) = key.checked_sub(1).and_then(|index| watches.get_mut(index)) {
            if watch.pending && ptr::eq(overlapped, watch.overlapped.as_mut()) {
                watch.pending = false;
                remaining -= 1;
            }
        }
    }
}

fn translate_buffer(
    buffer: &[u8],
    directory: &Path,
    exact_file: Option<&Path>,
) -> notify::Result<Vec<Event>> {
    let mut events = Vec::new();
    let mut offset = 0usize;
    let filename_offset = std::mem::offset_of!(FILE_NOTIFY_INFORMATION, FileName);

    loop {
        if offset + filename_offset > buffer.len() {
            return Err(notify::Error::generic(
                "ReadDirectoryChangesW returned a truncated notification header",
            ));
        }
        let entry = unsafe {
            ptr::read_unaligned(
                buffer
                    .as_ptr()
                    .add(offset)
                    .cast::<FILE_NOTIFY_INFORMATION>(),
            )
        };
        let filename_bytes = entry.FileNameLength as usize;
        let filename_start = offset + filename_offset;
        let filename_end = filename_start.saturating_add(filename_bytes);
        if filename_bytes % 2 != 0 || filename_end > buffer.len() {
            return Err(notify::Error::generic(
                "ReadDirectoryChangesW returned a truncated filename",
            ));
        }
        let filename = unsafe {
            std::slice::from_raw_parts(
                buffer.as_ptr().add(filename_start).cast::<u16>(),
                filename_bytes / 2,
            )
        };
        let path = directory.join(PathBuf::from(OsString::from_wide(filename)));
        if exact_file.is_none_or(|expected| expected == path) {
            if let Some(kind) = event_kind(entry.Action) {
                events.push(Event::new(kind).add_path(path));
            }
        }

        if entry.NextEntryOffset == 0 {
            break;
        }
        let next = entry.NextEntryOffset as usize;
        if next < filename_offset || offset.saturating_add(next) >= buffer.len() {
            return Err(notify::Error::generic(
                "ReadDirectoryChangesW returned an invalid entry offset",
            ));
        }
        offset += next;
    }
    Ok(events)
}

fn event_kind(action: u32) -> Option<EventKind> {
    match action {
        FILE_ACTION_ADDED => Some(EventKind::Create(CreateKind::Any)),
        FILE_ACTION_REMOVED => Some(EventKind::Remove(RemoveKind::Any)),
        FILE_ACTION_MODIFIED => Some(EventKind::Modify(ModifyKind::Any)),
        FILE_ACTION_RENAMED_OLD_NAME => Some(EventKind::Modify(ModifyKind::Name(RenameMode::From))),
        FILE_ACTION_RENAMED_NEW_NAME => Some(EventKind::Modify(ModifyKind::Name(RenameMode::To))),
        _ => None,
    }
}

fn buffer_overflow_event(root: &Path) -> Event {
    Event::new(EventKind::Other)
        .set_flag(Flag::Rescan)
        .set_info("rescan: buffer overflow")
        .add_path(root.to_path_buf())
}

fn last_notify_error() -> notify::Error {
    notify::Error::io(io::Error::last_os_error())
}

fn os_notify_error(error: u32) -> notify::Error {
    notify::Error::io(io::Error::from_raw_os_error(error as i32))
}

#[derive(Default)]
struct DrainStats {
    completions: u64,
    events: u64,
    total_micros: u128,
    max_micros: u128,
    measure_to_stderr: bool,
}

impl DrainStats {
    fn new(measure_to_stderr: bool) -> Self {
        Self {
            measure_to_stderr,
            ..Self::default()
        }
    }

    fn note(&mut self, elapsed: Duration, event_count: usize, root: &Path) {
        let micros = elapsed.as_micros();
        self.completions += 1;
        self.events += event_count as u64;
        self.total_micros += micros;
        self.max_micros = self.max_micros.max(micros);
        if self.measure_to_stderr {
            self.emit("sample", root);
        }
    }

    fn log(&self, root: &Path) {
        let average_micros = self.average_micros();
        crate::slog_info!(
            "Windows watcher drain: root={} completions={} events={} average_us={} max_us={}",
            root.display(),
            self.completions,
            self.events,
            average_micros,
            self.max_micros
        );
    }

    fn emit(&self, label: &str, root: &Path) {
        eprintln!(
            "Windows watcher drain {label}: root={} completions={} events={} average_us={} max_us={}",
            root.display(),
            self.completions,
            self.events,
            self.average_micros(),
            self.max_micros
        );
    }

    fn average_micros(&self) -> u128 {
        if self.completions == 0 {
            0
        } else {
            self.total_micros / u128::from(self.completions)
        }
    }
}

#[cfg(test)]
static PAUSE_BACKEND_START: AtomicBool = AtomicBool::new(false);
#[cfg(test)]
static BACKEND_START_REACHED: AtomicBool = AtomicBool::new(false);
#[cfg(test)]
static PAUSE_COMPLETION_DRAIN: AtomicBool = AtomicBool::new(false);
#[cfg(test)]
static COMPLETION_DRAIN_REACHED: AtomicBool = AtomicBool::new(false);

#[cfg(test)]
fn pause_backend_start_for_test(shutdown: &AtomicBool) {
    BACKEND_START_REACHED.store(true, Ordering::Release);
    while PAUSE_BACKEND_START.load(Ordering::Acquire) && !shutdown.load(Ordering::Acquire) {
        thread::sleep(Duration::from_millis(1));
    }
}

#[cfg(not(test))]
fn pause_backend_start_for_test(_shutdown: &AtomicBool) {}

#[cfg(test)]
fn pause_completion_drain_for_test(shutdown: &AtomicBool) {
    COMPLETION_DRAIN_REACHED.store(true, Ordering::Release);
    while PAUSE_COMPLETION_DRAIN.load(Ordering::Acquire) && !shutdown.load(Ordering::Acquire) {
        thread::sleep(Duration::from_millis(1));
    }
}

#[cfg(not(test))]
fn pause_completion_drain_for_test(_shutdown: &AtomicBool) {}

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, OnceLock, RwLock};

    use super::*;

    fn native_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    struct PauseReset;

    impl Drop for PauseReset {
        fn drop(&mut self) {
            PAUSE_BACKEND_START.store(false, Ordering::Release);
            PAUSE_COMPLETION_DRAIN.store(false, Ordering::Release);
        }
    }

    fn wait_until(flag: &AtomicBool, label: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !flag.load(Ordering::Acquire) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(
            flag.load(Ordering::Acquire),
            "timed out waiting for {label}"
        );
    }

    #[test]
    fn windows_buffer_overflow_emits_exactly_one_typed_rescan() {
        let _lock = native_test_lock();
        let _reset = PauseReset;
        PAUSE_COMPLETION_DRAIN.store(true, Ordering::Release);
        COMPLETION_DRAIN_REACHED.store(false, Ordering::Release);

        let root = tempfile::tempdir().unwrap();
        let canonical_root = std::fs::canonicalize(root.path()).unwrap();
        let matcher = Arc::new(RwLock::new(None));
        let generation = Arc::new(AtomicU64::new(1));
        let shutdown = Arc::new(AtomicBool::new(false));
        let filter_shutdown = Arc::clone(&shutdown);
        let attach_matcher = Arc::clone(&matcher);
        let attach_generation = Arc::clone(&generation);
        let (dispatch_tx, dispatch_rx) = crate::watcher_filter::watcher_dispatch_channel();
        let filter_thread = thread::spawn(move || {
            crate::watcher_filter::run_watcher_thread(
                crate::watcher_filter::WatcherFilterConfig::new(canonical_root, None),
                Vec::new(),
                matcher,
                generation,
                dispatch_tx,
                filter_shutdown,
                move |root, extra_watch_paths, tx| {
                    ProjectWatcher::create(
                        root,
                        extra_watch_paths,
                        tx,
                        attach_matcher,
                        attach_generation,
                    )
                },
            );
        });
        wait_until(&COMPLETION_DRAIN_REACHED, "completion drain pause");

        for index in 0..5_000 {
            std::fs::write(
                root.path().join(format!(
                    "overflow-{index:05}-abcdefghijklmnopqrstuvwxyz0123456789.tmp"
                )),
                b"overflow",
            )
            .unwrap();
        }
        PAUSE_COMPLETION_DRAIN.store(false, Ordering::Release);

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut rescans = Vec::new();
        while Instant::now() < deadline {
            match dispatch_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(crate::watcher_filter::WatcherDispatchEvent::RescanRequired(reason)) => {
                    rescans.push(reason);
                    if rescans.len() > 1 {
                        break;
                    }
                }
                Ok(_) | Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            }
            if rescans.len() == 1 {
                thread::sleep(Duration::from_millis(250));
                while let Ok(event) = dispatch_rx.try_recv() {
                    if let crate::watcher_filter::WatcherDispatchEvent::RescanRequired(reason) =
                        event
                    {
                        rescans.push(reason);
                    }
                }
                break;
            }
        }
        shutdown.store(true, Ordering::Release);
        filter_thread.join().unwrap();

        assert_eq!(
            rescans,
            vec![crate::watcher_filter::RescanReason::BufferOverflow]
        );
    }

    #[test]
    fn windows_generation_bump_before_backend_start_is_observed() {
        let _lock = native_test_lock();
        let _reset = PauseReset;
        PAUSE_BACKEND_START.store(true, Ordering::Release);
        BACKEND_START_REACHED.store(false, Ordering::Release);

        let root = tempfile::tempdir().unwrap();
        let canonical_root = std::fs::canonicalize(root.path()).unwrap();
        let counters = crate::context::watcher_counters_for_root(&canonical_root);
        let matcher = Arc::new(RwLock::new(None));
        let generation = Arc::new(AtomicU64::new(1));
        let create_generation = Arc::clone(&generation);
        let create_root = canonical_root.clone();
        let (watcher_tx, watcher_rx) = mpsc::sync_channel(1);
        let create_thread = thread::spawn(move || {
            let (tx, _rx) = mpsc::channel();
            let result =
                ProjectWatcher::create(create_root, Vec::new(), tx, matcher, create_generation);
            watcher_tx.send(result).unwrap();
        });

        wait_until(&BACKEND_START_REACHED, "backend start pause");
        generation.store(2, Ordering::Release);
        PAUSE_BACKEND_START.store(false, Ordering::Release);
        let watcher = watcher_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("watcher startup")
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        while counters.backend_exclusions().matcher_generation != 2 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(counters.backend_exclusions().matcher_generation, 2);
        drop(watcher);
        create_thread.join().unwrap();
    }

    /// The completion thread exits by itself once its event receiver is gone.
    /// The watcher is still alive at that point and will post its shutdown
    /// packet on drop, so the port handle must still be open and still be
    /// this watcher's port. A thread that closes the handle on exit fails the
    /// handle-information assertion here instead of letting `Drop` post into
    /// whatever object reused the handle value.
    #[test]
    fn completion_port_outlives_a_self_terminated_completion_thread() {
        use windows_sys::Win32::Foundation::GetHandleInformation;

        let _lock = native_test_lock();
        let root = tempfile::tempdir().unwrap();
        let canonical_root = std::fs::canonicalize(root.path()).unwrap();
        let (tx, rx) = mpsc::channel();
        let watcher = ProjectWatcher::create(
            canonical_root,
            Vec::new(),
            tx,
            Arc::new(RwLock::new(None)),
            Arc::new(AtomicU64::new(1)),
        )
        .expect("watcher startup");

        // With the receiver gone, the next delivered event makes the
        // completion loop stop and the thread return.
        drop(rx);
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut index = 0usize;
        while !watcher.join.as_ref().unwrap().is_finished() && Instant::now() < deadline {
            std::fs::write(root.path().join(format!("exit-{index}.tmp")), b"x").unwrap();
            index += 1;
            thread::sleep(Duration::from_millis(20));
        }
        assert!(
            watcher.join.as_ref().unwrap().is_finished(),
            "completion thread should stop once its receiver is dropped"
        );

        let port = watcher.completion_port.raw();
        let mut flags = 0u32;
        assert_ne!(
            unsafe { GetHandleInformation(port, &mut flags) },
            0,
            "completion port was closed while the watcher can still post to it"
        );

        // Round-trip a packet to prove the handle still names this port.
        const PROBE_KEY: usize = 0x5eed;
        assert_ne!(
            unsafe { PostQueuedCompletionStatus(port, 0, PROBE_KEY, ptr::null()) },
            0
        );
        let mut bytes = 0u32;
        let mut key = 0usize;
        let mut overlapped = ptr::null_mut();
        let dequeued =
            unsafe { GetQueuedCompletionStatus(port, &mut bytes, &mut key, &mut overlapped, 1000) };
        assert_ne!(
            dequeued, 0,
            "probe packet should be dequeued from the same port"
        );
        assert_eq!(key, PROBE_KEY);
        assert!(overlapped.is_null());

        assert_eq!(Arc::strong_count(&watcher.completion_port), 1);
        drop(watcher);
    }
}
