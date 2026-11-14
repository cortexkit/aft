//! Opt-in search work counts and per-pass timings.
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

pub static FILE_READS: AtomicUsize = AtomicUsize::new(0);
pub fn record_file_read() {
    FILE_READS.fetch_add(1, Ordering::Relaxed);
    record(|counts| counts.file_reads += 1);
}

pub fn record_file_read_path(path: &Path) {
    record_file_read();
    let first = READ_PATHS.with(|paths| paths.borrow_mut().insert(path.to_path_buf()));
    record(|counts| {
        if first {
            counts.unique_read_paths += 1;
        } else {
            counts.repeated_path_reads += 1;
        }
    });
}

pub fn record_memo_file_read(path: &Path) {
    record_file_read_path(path);
    let first = MEMO_PATHS.with(|paths| paths.borrow_mut().insert(path.to_path_buf()));
    record(|counts| {
        counts.memo_digest_reads += 1;
        if first {
            counts.memo_unique_paths += 1;
        } else {
            counts.memo_repeated_path_reads += 1;
        }
    });
}

#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub struct Counts {
    pub file_reads: usize,
    pub regex_compilations: usize,
    pub score_evaluations: usize,
    pub candidates_sorted: usize,
    pub token_scans: usize,
    pub token_lines_scanned: usize,
    pub anchor_windows_evaluated: usize,
    pub reused_payload_clones: usize,
    pub refresh_entry_visits: usize,
    pub project_walks: usize,
    pub project_walk_entries: usize,
    pub project_walk_files: usize,
    pub exact_passes: usize,
    pub exact_preparations: usize,
    pub exact_pass_candidates: usize,
    pub exact_pass_reads: usize,
    pub exact_pass_micros: usize,
    pub unique_read_paths: usize,
    pub repeated_path_reads: usize,
    pub memo_digest_reads: usize,
    pub memo_unique_paths: usize,
    pub memo_repeated_path_reads: usize,
}

thread_local! {
    static COUNTS: Cell<Counts> = Cell::new(Counts::default());
    static READ_PATHS: RefCell<HashSet<PathBuf>> = RefCell::new(HashSet::new());
    static MEMO_PATHS: RefCell<HashSet<PathBuf>> = RefCell::new(HashSet::new());
}

/// Start a thread-local measurement window, including repeated-path counts.
pub fn reset() {
    COUNTS.with(|cell| cell.set(Counts::default()));
    READ_PATHS.with(|paths| paths.borrow_mut().clear());
    MEMO_PATHS.with(|paths| paths.borrow_mut().clear());
}

pub fn counts() -> Counts {
    COUNTS.with(Cell::get)
}

pub fn record(update: impl FnOnce(&mut Counts)) {
    COUNTS.with(|cell| {
        let mut counts = cell.get();
        update(&mut counts);
        cell.set(counts);
    });
}
