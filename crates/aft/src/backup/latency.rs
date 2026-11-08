//! Where the time of one undo snapshot and one source-file write goes.
//!
//! The per-call `edit_phases` log field says how long the backup and write
//! phases took, not why. These timers split both phases into the steps that
//! can wait: lock waits, device flushes, `aft.db` writes and the WAL
//! checkpoints SQLite may run inside a commit. Each thread keeps its own
//! totals, so concurrent tool calls never mix their numbers. A slow edit logs
//! the split (see [`log_if_slow`]), and the `edit_latency_probe` example reads
//! it after every replayed edit.

use std::cell::RefCell;
use std::time::{Duration, Instant};

/// One step of the backup or write phase.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Part {
    /// Waiting for the process-wide backup store mutex.
    StoreLock,
    /// Pending-discard retries, legacy-layout checks and once-per-process
    /// backup maintenance that run before a snapshot.
    Maintenance,
    /// Taking the backup namespace, session and per-file stack locks.
    StackLock,
    /// Re-reading the file's on-disk undo stack before appending to it.
    Hydrate,
    /// Reading the file being backed up, when the caller had not already.
    Capture,
    /// Creating, writing, syncing and renaming the snapshot's content file.
    ContentFile,
    /// Writing, syncing and renaming the stack's `meta.json`.
    MetaFile,
    /// Syncing the stack directory so both renames are durable.
    DirSync,
    /// The one device-cache flush that ends a grouped snapshot.
    DeviceFlush,
    /// Writing the unsynced `post-state.json` annotation.
    PostState,
    /// Removing content files the stack no longer references.
    Prune,
    /// Waiting for the shared `aft.db` connection mutex.
    DbLock,
    /// Running and committing the `aft.db` mirror transaction, excluding
    /// any WAL checkpoint the commit ran (see [`Part::WalCheckpoint`]).
    DbWrite,
    /// A WAL checkpoint run inside an `aft.db` commit on this thread.
    WalCheckpoint,
    /// Creating the temporary source file, copying access metadata and
    /// writing the new bytes.
    WriteTemp,
    /// Syncing the new source bytes before they replace the original.
    WriteSync,
    /// Renaming the temporary over the original.
    WriteRename,
    /// Syncing the source file's directory after the rename.
    WriteDirSync,
}

const PART_COUNT: usize = 18;

impl Part {
    pub const ALL: [Part; PART_COUNT] = [
        Part::StoreLock,
        Part::Maintenance,
        Part::StackLock,
        Part::Hydrate,
        Part::Capture,
        Part::ContentFile,
        Part::MetaFile,
        Part::DirSync,
        Part::DeviceFlush,
        Part::PostState,
        Part::Prune,
        Part::DbLock,
        Part::DbWrite,
        Part::WalCheckpoint,
        Part::WriteTemp,
        Part::WriteSync,
        Part::WriteRename,
        Part::WriteDirSync,
    ];

    /// Steps of the backup phase, in the order a snapshot runs them.
    pub const BACKUP: [Part; 14] = [
        Part::StoreLock,
        Part::Maintenance,
        Part::StackLock,
        Part::Hydrate,
        Part::Capture,
        Part::ContentFile,
        Part::MetaFile,
        Part::DirSync,
        Part::DeviceFlush,
        Part::PostState,
        Part::Prune,
        Part::DbLock,
        Part::DbWrite,
        Part::WalCheckpoint,
    ];

    /// Steps of the write phase, in the order a replacement write runs them.
    pub const WRITE: [Part; 4] = [
        Part::WriteTemp,
        Part::WriteSync,
        Part::WriteRename,
        Part::WriteDirSync,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Part::StoreLock => "store_lock",
            Part::Maintenance => "maintenance",
            Part::StackLock => "stack_lock",
            Part::Hydrate => "hydrate",
            Part::Capture => "capture",
            Part::ContentFile => "content_file",
            Part::MetaFile => "meta_file",
            Part::DirSync => "dir_sync",
            Part::DeviceFlush => "device_flush",
            Part::PostState => "post_state",
            Part::Prune => "prune",
            Part::DbLock => "db_lock",
            Part::DbWrite => "db_write",
            Part::WalCheckpoint => "wal_checkpoint",
            Part::WriteTemp => "write_temp",
            Part::WriteSync => "write_sync",
            Part::WriteRename => "write_rename",
            Part::WriteDirSync => "write_dir_sync",
        }
    }

    const fn index(self) -> usize {
        self as usize
    }
}

/// Totals per [`Part`] taken from one thread.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PartTimes([Duration; PART_COUNT]);

impl PartTimes {
    pub fn get(&self, part: Part) -> Duration {
        self.0[part.index()]
    }

    pub fn set(&mut self, part: Part, value: Duration) {
        self.0[part.index()] = value;
    }

    /// `name:milliseconds` pairs for `parts`, with one decimal, skipping
    /// steps that took no measurable time.
    pub fn render(&self, parts: &[Part]) -> String {
        let rendered = parts
            .iter()
            .filter(|part| self.get(**part) >= Duration::from_micros(50))
            .map(|part| {
                format!(
                    "{}:{:.1}",
                    part.name(),
                    self.get(*part).as_secs_f64() * 1_000.0
                )
            })
            .collect::<Vec<_>>();
        if rendered.is_empty() {
            "none".to_string()
        } else {
            rendered.join(",")
        }
    }
}

thread_local! {
    static TIMES: RefCell<PartTimes> = const { RefCell::new(PartTimes([Duration::ZERO; PART_COUNT])) };
}

/// Add `elapsed` to this thread's total for `part`.
pub(crate) fn add(part: Part, elapsed: Duration) {
    TIMES.with(|times| times.borrow_mut().0[part.index()] += elapsed);
}

/// Adds the time until it is dropped to one [`Part`] on this thread.
pub(crate) struct Timer {
    part: Part,
    started: Instant,
}

impl Drop for Timer {
    fn drop(&mut self) {
        add(self.part, self.started.elapsed());
    }
}

/// Start timing `part`; see [`Timer`].
pub(crate) fn time(part: Part) -> Timer {
    Timer {
        part,
        started: Instant::now(),
    }
}

/// This thread's totals since the last call, then clear them.
pub fn take() -> PartTimes {
    TIMES.with(|times| std::mem::take(&mut *times.borrow_mut()))
}

/// Phases slower than this log their split.
const SLOW_PHASE: Duration = Duration::from_millis(100);

/// Log the split of a backup or write phase that took at least
/// [`SLOW_PHASE`], so a slow edit in a live daemon names the step it waited
/// on. Clears this thread's totals either way.
pub(crate) fn log_if_slow(phase: &str, parts: &[Part], elapsed: Duration, path: &std::path::Path) {
    let times = take();
    if elapsed >= SLOW_PHASE {
        crate::slog_info!(
            "slow edit {} phase: {}ms for {} ({})",
            phase,
            elapsed.as_millis(),
            path.display(),
            times.render(parts)
        );
    }
}
