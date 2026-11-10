//! Names the operation behind large or bursty heap use in the daemon.
//!
//! The `aft` binary installs [`TracingAllocator`] as its global allocator. It
//! forwards every call to the system allocator and watches two things:
//!
//! - One allocation or reallocation of [`LARGE_ALLOCATION_BYTES`] or more
//!   records an event: its size, the old size for a reallocation, the
//!   thread, the current operation label and the caller's return addresses.
//! - Allocations made while an operation label is set (see [`scope`]) add to
//!   that label's live bytes, and frees made under the same label subtract
//!   from them. When one label's live bytes cross [`LABEL_BURST_BYTES`] an
//!   event is recorded, and another when they fall back below half of it.
//!   A label's count starts again from zero whenever no scope of it is open,
//!   so memory an operation hands to someone else does not pile up on it
//!   over the daemon's life.
//!
//! The allocator never allocates, locks or logs on behalf of a caller. Its
//! common path is a thread-local add; every [`FLUSH_QUANTUM_BYTES`] of net
//! change it adds the thread's share to the label's shared counter with a
//! relaxed atomic. Events go into a fixed ring that a logger thread drains,
//! names and writes as WARN lines (see `report`).
//!
//! Only Rust heap allocations pass through a global allocator. Memory that C
//! libraries (SQLite, tree-sitter, ONNX Runtime) take from `malloc` directly
//! is not seen here.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::marker::PhantomData;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;

mod capture;
mod report;
mod ring;

pub use report::{drain_now, set_line_sink, start_logger};

/// A single allocation at least this large is recorded with its backtrace.
pub const LARGE_ALLOCATION_BYTES: usize = 64 << 20;
/// One label's live bytes crossing this records a burst event.
pub const LABEL_BURST_BYTES: isize = 512 << 20;
/// A thread hands its pending bytes to the label's shared counter whenever
/// they move this far from zero, so the shared counter lags each thread by
/// less than this.
pub const FLUSH_QUANTUM_BYTES: isize = 1 << 20;

/// Labels the process can name. Fixed labels take the first ids; tool labels
/// are interned after them until the table is full, and later tool names
/// share [`Label::TOOL_OTHER`].
const MAX_LABELS: usize = 128;

/// An operation the allocator attributes live bytes to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Label(u8);

const FIXED_LABELS: [&str; 11] = [
    "",
    "index_build",
    "callgraph_start",
    "callgraph_refresh",
    "segment_fold",
    "meaning_fill",
    "symbol_prewarm",
    "remote_input_capture",
    "frame_encode",
    "census",
    "tool:other",
];

/// Labels interned at run time, in id order after the fixed ones. Only label
/// creation and the logger read it; the allocator never does.
static INTERNED: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());

impl Label {
    /// No operation: allocations are not attributed.
    pub const NONE: Label = Label(0);
    /// A data-layer build of a checkout's indexes.
    pub const INDEX_BUILD: Label = Label(1);
    /// Starting a checkout's live call-graph engine.
    pub const CALLGRAPH_START: Label = Label(2);
    /// Resolving the call graph again after edits.
    pub const CALLGRAPH_REFRESH: Label = Label(3);
    /// Folding a checkout's stored segments into one.
    pub const SEGMENT_FOLD: Label = Label(4);
    /// One round of filling the meaning-search vectors.
    pub const MEANING_FILL: Label = Label(5);
    /// Warming a checkout's symbol cache after it binds.
    pub const SYMBOL_PREWARM: Label = Label(6);
    /// Capturing the inputs of a remote shell job.
    pub const REMOTE_INPUT_CAPTURE: Label = Label(7);
    /// Encoding a reply frame for the transport.
    pub const FRAME_ENCODE: Label = Label(8);
    /// Measuring the memory census.
    pub const CENSUS: Label = Label(9);
    /// A tool call whose name did not fit in the label table.
    pub const TOOL_OTHER: Label = Label(10);

    /// The label for a call of the tool `name`, as `tool:<name>`.
    pub fn tool(name: &str) -> Label {
        let valid = !name.is_empty()
            && name.len() <= 48
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'));
        if !valid {
            return Label::TOOL_OTHER;
        }
        Label::intern_with_prefix("tool:", name).unwrap_or(Label::TOOL_OTHER)
    }

    /// A label with any name, interned like a tool label. For tests and
    /// probes that need a label no production path uses.
    #[doc(hidden)]
    pub fn custom(name: &str) -> Label {
        Label::intern_with_prefix("", name).unwrap_or(Label::TOOL_OTHER)
    }

    fn intern_with_prefix(prefix: &str, name: &str) -> Option<Label> {
        let matches = |entry: &str| entry.strip_prefix(prefix) == Some(name);
        if let Some(index) = FIXED_LABELS.iter().position(|entry| matches(entry)) {
            return Some(Label(index as u8));
        }
        let mut interned = INTERNED.lock().unwrap_or_else(|e| e.into_inner());
        let offset = FIXED_LABELS.len();
        if let Some(index) = interned.iter().position(|entry| matches(entry)) {
            return Some(Label((offset + index) as u8));
        }
        if offset + interned.len() >= MAX_LABELS {
            return None;
        }
        let leaked: &'static str = Box::leak(format!("{prefix}{name}").into_boxed_str());
        interned.push(leaked);
        Some(Label((offset + interned.len() - 1) as u8))
    }

    /// The label's name; empty for [`Label::NONE`].
    pub fn name(self) -> &'static str {
        label_name(self.0)
    }
}

fn label_name(id: u8) -> &'static str {
    let id = usize::from(id);
    if let Some(name) = FIXED_LABELS.get(id) {
        return name;
    }
    INTERNED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(id - FIXED_LABELS.len())
        .copied()
        .unwrap_or("unknown")
}

/// The repository an operation works on, kept inline so that the allocator
/// can copy it into an event without allocating: the last component of the
/// root path, cut to fit.
#[derive(Clone, Copy)]
pub struct RootName {
    len: u8,
    bytes: [u8; 47],
}

impl RootName {
    pub const EMPTY: RootName = RootName {
        len: 0,
        bytes: [0; 47],
    };

    pub fn from_path(root: &Path) -> RootName {
        let name = root
            .file_name()
            .map(|name| name.to_string_lossy())
            .unwrap_or_else(|| root.to_string_lossy());
        RootName::from_str(&name)
    }

    fn from_str(name: &str) -> RootName {
        let mut end = name.len().min(47);
        while !name.is_char_boundary(end) {
            end -= 1;
        }
        let mut out = RootName::EMPTY;
        out.bytes[..end].copy_from_slice(&name.as_bytes()[..end]);
        out.len = end as u8;
        out
    }

    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.bytes[..usize::from(self.len)]).unwrap_or("")
    }
}

/// Shared counters of one label.
struct LabelState {
    /// Net bytes allocated minus freed under the label since it was last idle.
    live: AtomicIsize,
    /// Highest `live` since the previous census.
    census_peak: AtomicIsize,
    /// Highest `live` since it crossed [`LABEL_BURST_BYTES`].
    burst_peak: AtomicIsize,
    /// Open scopes of the label, across threads. Carried scopes in pool
    /// tasks do not count: their owner's scope covers them.
    active: AtomicU32,
    /// Whether the label is above the burst threshold, so that a crossing is
    /// reported once until it falls back.
    crossed: AtomicBool,
    /// When the current burst started, in nanoseconds of [`monotonic_ns`].
    crossed_at_ns: AtomicU64,
}

impl LabelState {
    const fn new() -> Self {
        Self {
            live: AtomicIsize::new(0),
            census_peak: AtomicIsize::new(0),
            burst_peak: AtomicIsize::new(0),
            active: AtomicU32::new(0),
            crossed: AtomicBool::new(false),
            crossed_at_ns: AtomicU64::new(0),
        }
    }
}

static LABELS: [LabelState; MAX_LABELS] = [const { LabelState::new() }; MAX_LABELS];

/// Per-thread tracing state. Every field is a `Cell` with a const initial
/// value and no destructor, so reading it inside the allocator never
/// allocates and still works while the thread's other locals are torn down.
struct ThreadState {
    /// The label new allocations are attributed to.
    label: Cell<u8>,
    /// The label `pending` belongs to. It lags `label` after a scope change
    /// until the next allocation, which hands the old label its bytes.
    pending_label: Cell<u8>,
    /// Net bytes not yet added to `pending_label`'s shared counter.
    pending: Cell<isize>,
    /// Set while this thread records an event or runs the logger, so that
    /// nothing done there is traced again.
    busy: Cell<bool>,
    root: Cell<RootName>,
}

thread_local! {
    static THREAD: ThreadState = const {
        ThreadState {
            label: Cell::new(0),
            pending_label: Cell::new(0),
            pending: Cell::new(0),
            busy: Cell::new(false),
            root: Cell::new(RootName::EMPTY),
        }
    };
}

/// The global allocator wrapper the `aft` binary installs. It forwards to
/// [`System`] and reports to this module after each call.
pub struct TracingAllocator;

unsafe impl GlobalAlloc for TracingAllocator {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            on_alloc(layout.size());
        }
        ptr
    }

    #[inline]
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            on_alloc(layout.size());
        }
        ptr
    }

    #[inline]
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        account(-(layout.size() as isize));
    }

    #[inline]
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let next = unsafe { System.realloc(ptr, layout, new_size) };
        if !next.is_null() {
            if new_size >= LARGE_ALLOCATION_BYTES {
                record_large(EventKind::Realloc, new_size, layout.size());
            }
            account(new_size as isize - layout.size() as isize);
        }
        next
    }
}

#[inline(always)]
fn on_alloc(size: usize) {
    if size >= LARGE_ALLOCATION_BYTES {
        record_large(EventKind::Alloc, size, 0);
    }
    account(size as isize);
}

/// Adds `delta` to the current label's pending bytes on this thread.
#[inline(always)]
fn account(delta: isize) {
    THREAD.with(|thread| {
        let label = thread.label.get();
        if label != thread.pending_label.get() {
            switch_pending(thread, label, delta);
            return;
        }
        if label == 0 {
            return;
        }
        let pending = thread.pending.get() + delta;
        if pending >= FLUSH_QUANTUM_BYTES || pending <= -FLUSH_QUANTUM_BYTES {
            thread.pending.set(0);
            flush(thread, label, pending);
        } else {
            thread.pending.set(pending);
        }
    });
}

/// The thread's label changed since its last allocation: hand the old label
/// what it is owed and start counting for the new one.
#[cold]
fn switch_pending(thread: &ThreadState, label: u8, delta: isize) {
    let old = thread.pending_label.get();
    let owed = thread.pending.replace(0);
    if old != 0 && owed != 0 {
        flush(thread, old, owed);
    }
    thread.pending_label.set(label);
    if label == 0 {
        return;
    }
    if delta >= FLUSH_QUANTUM_BYTES || delta <= -FLUSH_QUANTUM_BYTES {
        flush(thread, label, delta);
    } else {
        thread.pending.set(delta);
    }
}

/// Adds `delta` to `label`'s shared counter and records a burst crossing or
/// its end.
#[cold]
fn flush(thread: &ThreadState, label: u8, delta: isize) {
    let state = &LABELS[usize::from(label)];
    let live = state.live.fetch_add(delta, Ordering::Relaxed) + delta;
    if delta > 0 {
        state.census_peak.fetch_max(live, Ordering::Relaxed);
        if live >= LABEL_BURST_BYTES {
            state.burst_peak.fetch_max(live, Ordering::Relaxed);
            if !state.crossed.load(Ordering::Relaxed)
                && !state.crossed.swap(true, Ordering::Relaxed)
            {
                state.crossed_at_ns.store(monotonic_ns(), Ordering::Relaxed);
                record_burst(thread, label, live);
            }
        }
    } else if live < LABEL_BURST_BYTES / 2 && state.crossed.load(Ordering::Relaxed) {
        end_burst(label, thread.root.get());
    }
}

/// Records the end of `label`'s burst if this caller is the one to clear it.
fn end_burst(label: u8, root: RootName) {
    let state = &LABELS[usize::from(label)];
    if !state.crossed.swap(false, Ordering::Relaxed) {
        return;
    }
    let peak = state.burst_peak.swap(0, Ordering::Relaxed);
    let started = state.crossed_at_ns.load(Ordering::Relaxed);
    let mut event = ring::Event::new(EventKind::BurstEnded);
    event.label = label;
    event.root = root;
    event.size = peak.max(0) as usize;
    event.old_size = state.live.load(Ordering::Relaxed).max(0) as usize;
    event.duration_ns = monotonic_ns().saturating_sub(started);
    ring::push(&mut event);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EventKind {
    Alloc,
    Realloc,
    BurstCrossed,
    BurstEnded,
}

/// Records one large allocation. Rare, so it may take a backtrace and read
/// the thread's name, but it still must not allocate.
#[cold]
#[inline(never)]
fn record_large(kind: EventKind, size: usize, old_size: usize) {
    THREAD.with(|thread| {
        if thread.busy.replace(true) {
            return;
        }
        let mut event = ring::Event::new(kind);
        event.size = size;
        event.old_size = old_size;
        event.label = thread.label.get();
        event.root = thread.root.get();
        capture::fill(&mut event);
        ring::push(&mut event);
        thread.busy.set(false);
    });
}

#[cold]
#[inline(never)]
fn record_burst(thread: &ThreadState, label: u8, live: isize) {
    if thread.busy.replace(true) {
        return;
    }
    let mut event = ring::Event::new(EventKind::BurstCrossed);
    event.size = live.max(0) as usize;
    event.label = label;
    event.root = thread.root.get();
    capture::fill(&mut event);
    ring::push(&mut event);
    thread.busy.set(false);
}

fn monotonic_ns() -> u64 {
    static EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    let epoch = *EPOCH.get_or_init(std::time::Instant::now);
    u64::try_from(epoch.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

/// While alive, allocations on this thread are attributed to its label.
/// Dropping it restores the label and root that were current before.
#[must_use = "the label applies only while the guard lives"]
pub struct ScopeGuard {
    previous_label: u8,
    previous_root: RootName,
    label: u8,
    /// Whether this scope counts toward the label's open scopes; a carried
    /// scope in a pool task does not.
    owner: bool,
    _not_send: PhantomData<*const ()>,
}

/// Attributes this thread's allocations to `label` until the guard drops.
/// With a `root`, events name that repository; without one the current
/// root, if any, stays.
pub fn scope(label: Label, root: Option<&Path>) -> ScopeGuard {
    let root = root.map(RootName::from_path);
    enter(label.0, root, true)
}

fn enter(label: u8, root: Option<RootName>, owner: bool) -> ScopeGuard {
    if owner && label != 0 {
        LABELS[usize::from(label)]
            .active
            .fetch_add(1, Ordering::Relaxed);
    }
    THREAD.with(|thread| {
        let previous_label = thread.label.replace(label);
        let previous_root = thread.root.get();
        if let Some(root) = root {
            thread.root.set(root);
        }
        ScopeGuard {
            previous_label,
            previous_root,
            label,
            owner,
            _not_send: PhantomData,
        }
    })
}

impl Drop for ScopeGuard {
    fn drop(&mut self) {
        let root = THREAD.with(|thread| {
            let root = thread.root.get();
            if self.owner && thread.pending_label.get() == self.label {
                // The label may go idle below; hand it this thread's bytes
                // first so they are not added after its count restarts.
                let owed = thread.pending.replace(0);
                if self.label != 0 && owed != 0 {
                    flush(thread, self.label, owed);
                }
            }
            thread.label.set(self.previous_label);
            thread.root.set(self.previous_root);
            root
        });
        if !self.owner || self.label == 0 {
            return;
        }
        close_scope(self.label, root);
    }
}

/// The current thread's label and root, to carry into work handed to a pool.
#[derive(Clone, Copy)]
pub struct Carry {
    label: u8,
    root: RootName,
}

/// Captures the current label and root for a pool task; see [`Carry::enter`].
pub fn carry() -> Carry {
    THREAD.with(|thread| Carry {
        label: thread.label.get(),
        root: thread.root.get(),
    })
}

/// `pool.install(op)`, with `op` run under the caller's label. Parallel
/// iterators inside `op` carry it further with [`Carry::each`].
pub fn install<R: Send>(pool: &rayon::ThreadPool, op: impl FnOnce() -> R + Send) -> R {
    pool.install(carry().wrap(op))
}

/// `inner`, with every poll run under `label`. For async work, where a
/// [`ScopeGuard`] cannot be held across an await: the label is open from the
/// first poll until the future is dropped.
pub fn future<F: std::future::Future>(label: Label, root: Option<&Path>, inner: F) -> Labelled<F> {
    Labelled {
        inner,
        carry: Carry {
            label: label.0,
            root: root.map_or_else(|| carry().root, RootName::from_path),
        },
        open: false,
    }
}

/// A future whose polls run under a label; see [`future`].
pub struct Labelled<F> {
    inner: F,
    carry: Carry,
    /// Whether this future counts as an open scope of its label.
    open: bool,
}

impl<F: std::future::Future> std::future::Future for Labelled<F> {
    type Output = F::Output;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<F::Output> {
        // SAFETY: `inner` is never moved out of the pinned wrapper; the other
        // fields are plain data.
        let this = unsafe { self.get_unchecked_mut() };
        if !this.open && this.carry.label != 0 {
            this.open = true;
            LABELS[usize::from(this.carry.label)]
                .active
                .fetch_add(1, Ordering::Relaxed);
        }
        let _label = this.carry.enter();
        // SAFETY: as above.
        unsafe { std::pin::Pin::new_unchecked(&mut this.inner) }.poll(cx)
    }
}

impl<F> Drop for Labelled<F> {
    fn drop(&mut self) {
        if self.open {
            close_scope(self.carry.label, self.carry.root);
        }
    }
}

/// One open scope of `label` closed. When it was the last, the label's live
/// count is reset to zero: no operation of the label is running, so any
/// bytes still counted are results it returned to other code, and keeping
/// them would let the count grow over the daemon's life.
fn close_scope(label: u8, root: RootName) {
    let state = &LABELS[usize::from(label)];
    if state.active.fetch_sub(1, Ordering::Relaxed) == 1 {
        state.live.store(0, Ordering::Relaxed);
        end_burst(label, root);
    }
}

impl Carry {
    /// Attributes the pool thread's allocations to the carried label until
    /// the guard drops. The task's owner keeps the label open meanwhile.
    pub fn enter(&self) -> ScopeGuard {
        enter(self.label, Some(self.root), false)
    }

    /// `work`, run under the carried label wherever it runs.
    pub fn wrap<R>(self, work: impl FnOnce() -> R) -> impl FnOnce() -> R {
        move || {
            let _label = self.enter();
            work()
        }
    }

    /// `work` for each item of a parallel iterator, each call run under the
    /// carried label on whichever pool thread takes the item.
    pub fn each<T, R>(self, work: impl Fn(T) -> R) -> impl Fn(T) -> R {
        move |item| {
            let _label = self.enter();
            work(item)
        }
    }

    pub fn label(&self) -> Label {
        Label(self.label)
    }
}

/// One label's high-water mark for the memory census.
#[derive(Clone, Debug, serde::Serialize, PartialEq, Eq)]
pub struct LabelPeak {
    pub label: &'static str,
    /// Highest live bytes since the previous census.
    pub peak_bytes: u64,
    /// Live bytes now.
    pub live_bytes: u64,
}

/// The `limit` labels with the highest live bytes since the previous call,
/// highest first; each label's mark then restarts from its current count.
/// Labels that never held a byte are left out. Empty unless the logger runs,
/// which only the `aft` binary starts: without the tracer installed nothing
/// counts real allocations, and unit tests that drive the hooks by hand must
/// not leak their labels into other tests' census.
pub fn take_census_peaks(limit: usize) -> Vec<LabelPeak> {
    if !report::logger_started() {
        return Vec::new();
    }
    collect_census_peaks(limit)
}

fn collect_census_peaks(limit: usize) -> Vec<LabelPeak> {
    let mut peaks = Vec::new();
    for (id, state) in LABELS.iter().enumerate().skip(1) {
        let live = state.live.load(Ordering::Relaxed).max(0);
        let peak = state.census_peak.swap(live, Ordering::Relaxed).max(live);
        if peak > 0 {
            peaks.push((id as u8, peak as u64, live as u64));
        }
    }
    peaks.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    peaks.truncate(limit);
    peaks
        .into_iter()
        .map(|(id, peak_bytes, live_bytes)| LabelPeak {
            label: label_name(id),
            peak_bytes,
            live_bytes,
        })
        .collect()
}

/// Live bytes attributed to `label` right now, including this thread's
/// pending share.
#[doc(hidden)]
pub fn live_bytes(label: Label) -> isize {
    let pending = THREAD.with(|thread| {
        if thread.pending_label.get() == label.0 {
            thread.pending.get()
        } else {
            0
        }
    });
    LABELS[usize::from(label.0)].live.load(Ordering::Relaxed) + pending
}

#[cfg(test)]
mod tests;
