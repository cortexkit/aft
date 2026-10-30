//! Peak heap of a cold trigram build, per file.
//!
//! The text index's posting records are spilled to disk under a fixed budget,
//! so what grows with the number of files is the file table and every copy of
//! the file paths the build holds while it runs. An earlier measurement on a
//! root with a million files found about 1.7 KB per file at publish, because
//! the build held the walked path list, the file table, the path lookup, full
//! clones of both for the cache writer, and the cache header as one buffer.
//!
//! This test pins how fast the peak live heap of a cold build grows with the
//! number of files. It builds a root of N files, adds N more, builds again,
//! and asserts on the slope: (peak at 2N - peak at N) / N. The slope leaves
//! out the build's fixed costs (worker pool, walker state, buffers), so a
//! small root still shows the per-file cost, and one more copy of every path
//! still fails the bound. By default N is 20,000, which runs in seconds; set
//! AFT_STRESS=1 for N = 125,000 (250,000 files at the second build).
//!
//! A counting global allocator in this test binary measures live bytes
//! exactly (allocations minus frees, across every thread, including the
//! build's worker pool), so the figures do not depend on the operating
//! system's page accounting or on timing. The binary holds only this test, so
//! no other test's allocations are counted.

use std::alloc::{GlobalAlloc, Layout, System};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use aft::search_index::SearchIndex;

/// Files in the first build; the second build has twice as many.
fn half_size() -> usize {
    if std::env::var("AFT_STRESS").as_deref() == Ok("1") {
        125_000
    } else {
        20_000
    }
}
const FILES_PER_DIR: usize = 20;

static LIVE_BYTES: AtomicUsize = AtomicUsize::new(0);
static PEAK_BYTES: AtomicUsize = AtomicUsize::new(0);

struct PeakAllocator;

fn grow(bytes: usize) {
    let live = LIVE_BYTES.fetch_add(bytes, Ordering::Relaxed) + bytes;
    PEAK_BYTES.fetch_max(live, Ordering::Relaxed);
}

// Live bytes go up on every allocation and down on every free, and the peak
// keeps the highest live total seen since the last reset.
unsafe impl GlobalAlloc for PeakAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            grow(layout.size());
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        LIVE_BYTES.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_pointer = unsafe { System.realloc(pointer, layout, new_size) };
        if !new_pointer.is_null() {
            LIVE_BYTES.fetch_sub(layout.size(), Ordering::Relaxed);
            grow(new_size);
        }
        new_pointer
    }
}

#[global_allocator]
static ALLOCATOR: PeakAllocator = PeakAllocator;

/// Writes files numbered `files`, twenty small files per leaf directory
/// across three directory levels, and returns their total path length.
fn write_tree(root: &Path, files: std::ops::Range<usize>) -> usize {
    let mut path_bytes = 0usize;
    for file in files {
        let leaf = file / FILES_PER_DIR;
        let dir = root
            .join(format!("project_directory_{:02}", leaf / 2_500))
            .join(format!("component_module_{:02}", (leaf / 50) % 50))
            .join(format!("feature_package_{:03}", leaf % 50));
        if file % FILES_PER_DIR == 0 {
            std::fs::create_dir_all(&dir).unwrap();
        }
        let path = dir.join(format!("source_file_number_{file:07}.txt"));
        std::fs::write(&path, format!("{file}\n")).unwrap();
        path_bytes += path.as_os_str().len();
    }
    path_bytes
}

/// Peak and retained live heap of one cold build of `root`, above the live
/// heap when the build started.
fn measure_build(root: &Path, expected_files: usize) -> (usize, usize) {
    let cache = tempfile::tempdir().unwrap();
    let baseline = LIVE_BYTES.load(Ordering::Relaxed);
    PEAK_BYTES.store(baseline, Ordering::Relaxed);
    let index = SearchIndex::build_with_limit_to_cache_dir(root, 1024 * 1024, cache.path());
    let peak = PEAK_BYTES.load(Ordering::Relaxed) - baseline;
    let retained = LIVE_BYTES.load(Ordering::Relaxed).saturating_sub(baseline);
    assert_eq!(index.files.len(), expected_files);
    println!(
        "cold build memory: files={expected_files} peak_bytes={peak} \
         peak_per_file={} retained_bytes={retained} retained_per_file={}",
        peak / expected_files,
        retained / expected_files
    );
    (peak, retained)
}

#[test]
fn cold_build_peak_heap_per_file_stays_bounded() {
    let half = half_size();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();

    write_tree(&root, 0..half);
    let (first_peak, _) = measure_build(&root, half);
    let added_path_bytes = write_tree(&root, half..2 * half);
    let (second_peak, _) = measure_build(&root, 2 * half);
    let average_path = added_path_bytes / half;

    let slope = second_peak.saturating_sub(first_peak) / half;
    println!(
        "cold build memory slope: files={half}->{} average_path_bytes={average_path} \
         peak_bytes_per_added_file={slope}",
        2 * half
    );

    // What the build holds for each extra file at its peak: the resident
    // file table and path lookup (two path copies, 2 x the average path,
    // plus about 150 bytes of entries) and the posting records not yet
    // spilled. On Linux that measured 511 B per added file at 20,000 ->
    // 40,000 files and 545 B at 125,000 -> 250,000, with 118-byte paths;
    // the difference is the record buffer's and the lookup's capacity
    // doubling. The bound sits between that and one more copy of every path
    // (at least the path plus a 24-byte PathBuf, about 650 B here), so a
    // build that keeps another copy of every path fails it. The build that
    // kept up to five copies measured 1,322 B per added file.
    let bound = 2 * average_path + 360;
    assert!(
        slope <= bound,
        "cold build peak grows {slope} B per added file, over {bound} B \
         (average path {average_path} B)"
    );
}
