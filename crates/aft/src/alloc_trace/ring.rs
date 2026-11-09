//! A fixed ring of trace events between the allocator and the logger.
//!
//! The allocator side claims a slot with one atomic add and one
//! compare-and-swap and copies the event in; when the logger has not emptied
//! that slot yet the event is dropped and counted instead of waiting. The
//! logger empties ready slots in sequence order.

use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};

use super::{EventKind, RootName};

/// Return addresses kept per event.
pub(crate) const MAX_FRAMES: usize = 48;
const SLOTS: usize = 64;

const EMPTY: u8 = 0;
const WRITING: u8 = 1;
const READY: u8 = 2;
const READING: u8 = 3;

#[derive(Clone, Copy)]
pub(crate) struct Event {
    pub(crate) seq: u64,
    pub(crate) kind: EventKind,
    /// The allocation's size; for a burst, the live bytes at the crossing or
    /// the peak at its end.
    pub(crate) size: usize,
    /// A reallocation's previous size; for a burst's end, the live bytes then.
    pub(crate) old_size: usize,
    pub(crate) label: u8,
    pub(crate) root: RootName,
    /// How long a burst lasted, for its end.
    pub(crate) duration_ns: u64,
    pub(crate) thread: [u8; 32],
    pub(crate) thread_len: u8,
    pub(crate) frames: [usize; MAX_FRAMES],
    pub(crate) frame_count: u8,
}

impl Event {
    pub(crate) fn new(kind: EventKind) -> Self {
        Self {
            seq: 0,
            kind,
            size: 0,
            old_size: 0,
            label: 0,
            root: RootName::EMPTY,
            duration_ns: 0,
            thread: [0; 32],
            thread_len: 0,
            frames: [0; MAX_FRAMES],
            frame_count: 0,
        }
    }

    pub(crate) fn thread_name(&self) -> &str {
        std::str::from_utf8(&self.thread[..usize::from(self.thread_len)]).unwrap_or("?")
    }

    pub(crate) fn frames(&self) -> &[usize] {
        &self.frames[..usize::from(self.frame_count)]
    }
}

struct Slot {
    state: AtomicU8,
    event: UnsafeCell<MaybeUninit<Event>>,
}

// SAFETY: a slot's event is written only by the thread that moved its state
// from EMPTY to WRITING and read only by the thread that moved it from READY
// to READING; the Release store that publishes each step orders the copy.
unsafe impl Sync for Slot {}

impl Slot {
    const fn new() -> Self {
        Self {
            state: AtomicU8::new(EMPTY),
            event: UnsafeCell::new(MaybeUninit::uninit()),
        }
    }
}

static RING: [Slot; SLOTS] = [const { Slot::new() }; SLOTS];
static NEXT: AtomicU64 = AtomicU64::new(0);
static DROPPED: AtomicU64 = AtomicU64::new(0);

/// Queues `event` for the logger and wakes it. Never blocks or allocates.
pub(crate) fn push(event: &mut Event) {
    let seq = NEXT.fetch_add(1, Ordering::Relaxed);
    event.seq = seq;
    let slot = &RING[(seq % SLOTS as u64) as usize];
    if slot
        .state
        .compare_exchange(EMPTY, WRITING, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        DROPPED.fetch_add(1, Ordering::Relaxed);
        return;
    }
    // SAFETY: this thread owns the slot until it publishes READY.
    unsafe { (*slot.event.get()).write(*event) };
    slot.state.store(READY, Ordering::Release);
    super::report::wake();
}

/// Takes every ready event, oldest first.
pub(crate) fn drain(out: &mut Vec<Event>) {
    let start = out.len();
    for slot in &RING {
        if slot
            .state
            .compare_exchange(READY, READING, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            // SAFETY: READY means the writer finished; this thread owns the
            // slot until it stores EMPTY.
            out.push(unsafe { (*slot.event.get()).assume_init() });
            slot.state.store(EMPTY, Ordering::Release);
        }
    }
    out[start..].sort_by_key(|event| event.seq);
}

/// Events dropped so far because their slot was still full.
pub(crate) fn dropped() -> u64 {
    DROPPED.load(Ordering::Relaxed)
}
