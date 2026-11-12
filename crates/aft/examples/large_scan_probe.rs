//! NDJSON handler replay with per-request peak Rust heap and wall time.
//!
//! Usage: `large_scan_probe <corpus-root> <storage-root>`; requests arrive on
//! stdin, and stdout contains a ready record followed by measured projections.
//! Run two revisions against the same corpus and alternate their requests.

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Counting;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static LARGEST: AtomicUsize = AtomicUsize::new(0);

fn allocated(size: usize) {
    let live = LIVE.fetch_add(size, Ordering::Relaxed) + size;
    PEAK.fetch_max(live, Ordering::Relaxed);
    LARGEST.fetch_max(size, Ordering::Relaxed);
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            allocated(layout.size());
        }
        pointer
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc_zeroed(layout) };
        if !pointer.is_null() {
            allocated(layout.size());
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) };
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let next = unsafe { System.realloc(pointer, layout, size) };
        if !next.is_null() {
            LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
            allocated(size);
        }
        next
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn main() {
    let args = std::env::args().collect::<Vec<_>>();
    let root = std::path::PathBuf::from(&args[1]).canonicalize().unwrap();
    let storage = std::path::PathBuf::from(&args[2]);
    std::fs::create_dir_all(&storage).unwrap();
    let ctx = aft::context::AppContext::new(
        Box::new(aft::parser::TreeSitterProvider::new()),
        aft::config::Config {
            storage_dir: Some(storage.clone()),
            ..Default::default()
        },
    );
    let request = serde_json::from_value(serde_json::json!({
        "id": "configure", "command": "configure", "harness": "opencode",
        "project_root": root, "storage_dir": storage,
        "config": [{"tier": "user", "doc": serde_json::json!({
            "indexes": {"trigram": true, "semantic": false, "callgraph": false}
        }).to_string()}]
    }))
    .unwrap();
    let configured = aft::commands::configure::handle_configure(&request, &ctx);
    assert!(configured.success, "{:?}", configured.data);
    aft::runtime_drain::drain_deferred_configure_maintenance(&ctx);
    println!("{{\"ready\":true}}");
    std::io::stdout().flush().unwrap();
    for line in std::io::stdin().lock().lines() {
        let request: aft::protocol::RawRequest = serde_json::from_str(&line.unwrap()).unwrap();
        let baseline = LIVE.load(Ordering::Relaxed);
        PEAK.store(baseline, Ordering::Relaxed);
        LARGEST.store(0, Ordering::Relaxed);
        let started = std::time::Instant::now();
        let response = match request.command.as_str() {
            "grep" => aft::commands::grep::handle_grep(&request, &ctx),
            "semantic_search" => {
                aft::commands::semantic_search::handle_semantic_search(&request, &ctx)
            }
            other => panic!("unsupported replay command {other}"),
        };
        let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
        let peak = PEAK.load(Ordering::Relaxed).saturating_sub(baseline);
        let largest = LARGEST.load(Ordering::Relaxed);
        // Compare user-visible text and ordered rows, not timing telemetry or
        // cache-work counters that intentionally change with the read strategy.
        let mut output = serde_json::json!({"success": response.success});
        for field in [
            "text",
            "results",
            "matches",
            "total_matches",
            "files_searched",
            "files_with_matches",
            "truncated",
        ] {
            if let Some(value) = response.data.get(field) {
                output[field] = value.clone();
            }
        }
        println!(
            "{}",
            serde_json::json!({"output": output, "elapsed_ms": elapsed_ms, "peak_heap_bytes": peak, "largest_allocation_bytes": largest})
        );
        std::io::stdout().flush().unwrap();
    }
}
