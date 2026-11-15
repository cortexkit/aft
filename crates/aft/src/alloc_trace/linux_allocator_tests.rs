use aft::memory::{MemoryEstimate, ProcessMemorySnapshot};
use std::collections::BTreeMap;

#[test]
fn linux_allocator_configuration_and_native_sqlite() {
    use tikv_jemalloc_ctl::{epoch, raw, stats};

    let background: bool = unsafe { raw::read(b"opt.background_thread\0") }.unwrap();
    let dirty: isize = unsafe { raw::read(b"opt.dirty_decay_ms\0") }.unwrap();
    let muzzy: isize = unsafe { raw::read(b"opt.muzzy_decay_ms\0") }.unwrap();
    assert!(background);
    assert_eq!(dirty, 10_000);
    assert_eq!(muzzy, 0);
    eprintln!("jemalloc options: background_thread={background}, dirty_decay_ms={dirty}, muzzy_decay_ms={muzzy}");

    // SQLite calls its C allocator, not Rust's GlobalAlloc implementation.
    // Holding a large C allocation must appear in jemalloc's own live counters.
    epoch::advance().unwrap();
    let before = stats::allocated::read().unwrap();
    const SIZE: usize = 64 * 1024 * 1024;
    let allocation = unsafe { rusqlite::ffi::sqlite3_malloc64(SIZE as u64) };
    assert!(!allocation.is_null());
    epoch::advance().unwrap();
    let during = stats::allocated::read().unwrap();
    unsafe { rusqlite::ffi::sqlite3_free(allocation) };
    assert!(
        during >= before + SIZE,
        "SQLite allocation missing from jemalloc: {before} -> {during}"
    );
    eprintln!("SQLite C allocation: requested={SIZE}, jemalloc allocated={before} -> {during}");

    let snapshot =
        ProcessMemorySnapshot::from_roots(&BTreeMap::new(), &MemoryEstimate::estimated(0));
    let jemalloc = snapshot.allocator.jemalloc.as_ref().unwrap();
    assert_eq!(jemalloc.accounting_source, "jemalloc");
    assert!(jemalloc.allocated > 0);
    assert!(jemalloc.allocated <= jemalloc.active);
    assert!(jemalloc.active <= jemalloc.resident);
    let (cached, _) = aft::memory::cached_allocator_observation().unwrap();
    assert_eq!(cached.jemalloc, snapshot.allocator.jemalloc);
    eprintln!(
        "process allocator: {}",
        serde_json::to_string(&snapshot.allocator).unwrap()
    );
}

#[test]
fn linux_allocator_stage_boundary_purge_measurement() {
    let mut pages = vec![0u8; 64 * 1024 * 1024];
    for page in pages.chunks_mut(4096) {
        page[0] = 1;
    }
    std::hint::black_box(&pages);
    drop(pages);
    let started = std::time::Instant::now();
    aft::memory::purge_allocator().expect("all-arena purge");
    eprintln!(
        "jemalloc stage-boundary purge after freeing 64 MiB: {} us",
        started.elapsed().as_micros()
    );
}
