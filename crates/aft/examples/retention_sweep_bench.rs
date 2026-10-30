//! Measures storage-wide blob GC passes over many view folders.
//!
//! Usage:
//!   cargo run --release -p agent-file-tools --example retention_sweep_bench -- \
//!     <scratch-dir> <views> <generations-per-view> <entries>[-<max-entries>] \
//!     <distinct-manifests> <passes> [reuse]
//!
//! `reuse` skips building the fixture and runs against the one already in
//! `<scratch-dir>`, in a new process: its first pass shows the cost after a
//! daemon restart, with sidecars on disk but nothing remembered in memory.
//!
//! Every view holds `generations-per-view` published manifests. To keep the
//! fixture's disk footprint small, the manifests are hard links to one of
//! `distinct-manifests` prototype files, each with its own keys and an entry
//! count drawn from the given range. The sweep visits every manifest path
//! independently, as it would separate files.
//!
//! For each pass the bench prints wall time, process CPU time (Unix only),
//! bytes read through read(2) (Linux `rchar`), heap traffic from a counting
//! allocator (total bytes allocated, allocation count, peak live heap above
//! the start of the pass), and the pass's peak resident set size (Linux
//! `VmHWM`, reset before each pass). A figure the platform cannot provide is
//! printed as `n/a`. At the end it prints the total size of the reference
//! sidecars on disk.
use std::{
    alloc::{GlobalAlloc, Layout, System},
    fs,
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
    time::Instant,
};

use aft::{
    blob_store::{BlobPlane, BlobStore},
    gc::{sweep, SweepReferences, SweepRequest, BLOB_AGE_FLOOR_MS},
    views::{Manifest, ManifestEntry, RegularPlanes, RelPath, ViewStore},
};

struct Counting;

static LIVE: AtomicU64 = AtomicU64::new(0);
static PEAK: AtomicU64 = AtomicU64::new(0);
static TOTAL: AtomicU64 = AtomicU64::new(0);
static COUNT: AtomicU64 = AtomicU64::new(0);

fn grew(bytes: u64) {
    let live = LIVE.fetch_add(bytes, Ordering::Relaxed) + bytes;
    PEAK.fetch_max(live, Ordering::Relaxed);
    TOTAL.fetch_add(bytes, Ordering::Relaxed);
    COUNT.fetch_add(1, Ordering::Relaxed);
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            grew(layout.size() as u64);
        }
        pointer
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc_zeroed(layout) };
        if !pointer.is_null() {
            grew(layout.size() as u64);
        }
        pointer
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) };
        LIVE.fetch_sub(layout.size() as u64, Ordering::Relaxed);
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let moved = unsafe { System.realloc(pointer, layout, size) };
        if !moved.is_null() {
            LIVE.fetch_sub(layout.size() as u64, Ordering::Relaxed);
            grew(size as u64);
        }
        moved
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Process CPU time (user plus system). Read with getrusage on Unix; other
/// platforms report it as not measured.
#[cfg(unix)]
fn cpu_seconds() -> Option<f64> {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: getrusage fills the provided struct for the calling process.
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
        return None;
    }
    let usage = unsafe { usage.assume_init() };
    let seconds = |time: libc::timeval| time.tv_sec as f64 + time.tv_usec as f64 / 1e6;
    Some(seconds(usage.ru_utime) + seconds(usage.ru_stime))
}

#[cfg(not(unix))]
fn cpu_seconds() -> Option<f64> {
    None
}

fn bytes_read() -> Option<u64> {
    let io = fs::read_to_string("/proc/self/io").ok()?;
    io.lines()
        .find_map(|line| line.strip_prefix("rchar: "))
        .and_then(|value| value.trim().parse().ok())
}

fn key(prototype: usize, entry: usize, plane: u8) -> [u8; 32] {
    *blake3::hash(format!("{prototype}/{entry}/{plane}").as_bytes()).as_bytes()
}

fn hex(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn prototype(index: usize, entries: usize) -> Vec<u8> {
    let manifest = Manifest::new((0..entries).map(|entry| {
        (
            RelPath::new(format!("src/module_{:03}/file_{entry:06}.rs", entry % 512).into_bytes())
                .unwrap(),
            ManifestEntry::Regular {
                mode: 0o100644,
                resolution_input: false,
                planes: RegularPlanes {
                    semantic: Some(hex(&key(index, entry, 0))),
                    callgraph: Some(hex(&key(index, entry, 1))),
                },
            },
        )
    }))
    .unwrap();
    manifest.to_json_bytes().unwrap()
}

fn seed_plane(storage: &Path, plane: BlobPlane, referenced: &[[u8; 32]], unreferenced: usize) {
    let store = BlobStore::open(storage, "family", plane).unwrap();
    let mut connection = rusqlite::Connection::open(store.path()).unwrap();
    drop(store);
    let tx = connection.transaction().unwrap();
    {
        let mut insert = tx
            .prepare("INSERT OR IGNORE INTO blob_payloads (full_key, payload, payload_digest, payload_schema, created_at_ms) VALUES (?1, X'00', ?1, 1, 0)")
            .unwrap();
        for key in referenced {
            insert.execute([key.as_slice()]).unwrap();
        }
        for index in 0..unreferenced {
            let key = *blake3::hash(format!("garbage/{index}").as_bytes()).as_bytes();
            insert.execute([key.as_slice()]).unwrap();
        }
    }
    tx.commit().unwrap();
    let rows: usize = connection
        .query_row("SELECT count(*) FROM blob_payloads", [], |row| row.get(0))
        .unwrap();
    assert!(
        rows >= unreferenced,
        "seeded {rows} rows, expected at least {unreferenced}"
    );
}

fn peak_rss_reset() {
    // Linux: writing 5 resets the peak resident set size to the current one.
    let _ = fs::write("/proc/self/clear_refs", "5");
}

fn peak_rss_mb() -> String {
    fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status.lines().find_map(|line| {
                line.strip_prefix("VmHWM:")
                    .and_then(|value| value.trim().strip_suffix("kB"))
                    .and_then(|value| value.trim().parse::<f64>().ok())
            })
        })
        .map_or_else(|| "n/a".into(), |kb| format!("{:.1}", kb / 1e3))
}

fn file_bytes(storage: &Path, extension: &str) -> (usize, u64) {
    let mut files = 0;
    let mut bytes = 0;
    for view in fs::read_dir(storage.join("views")).unwrap().flatten() {
        let Ok(entries) = fs::read_dir(view.path()) else {
            continue;
        };
        for entry in entries.flatten() {
            if entry.path().extension().is_some_and(|ext| ext == extension) {
                files += 1;
                bytes += entry.metadata().map_or(0, |metadata| metadata.len());
            }
        }
    }
    (files, bytes)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (args, reuse) = match args.split_last() {
        Some((last, rest)) if last == "reuse" => (rest.to_vec(), true),
        _ => (args, false),
    };
    let [root, views, generations, entries, distinct, passes] = args.as_slice() else {
        eprintln!("usage: retention_sweep_bench <scratch-dir> <views> <generations> <entries>[-<max-entries>] <distinct> <passes> [reuse]");
        std::process::exit(2);
    };
    let root = Path::new(root);
    let views: usize = views.parse().unwrap();
    let generations: usize = generations.parse().unwrap();
    let (min_entries, max_entries): (usize, usize) = match entries.split_once('-') {
        Some((min, max)) => (min.parse().unwrap(), max.parse().unwrap()),
        None => (entries.parse().unwrap(), entries.parse().unwrap()),
    };
    let distinct: usize = distinct.parse().unwrap();
    let passes: usize = passes.parse().unwrap();
    let storage = root.join("storage");
    if !reuse {
        let _ = fs::remove_dir_all(root);
        fs::create_dir_all(&storage).unwrap();
        fs::create_dir_all(storage.join("retention")).unwrap();
        let started = Instant::now();
        let mut sizes = Vec::new();
        for index in 0..distinct {
            let spread = max_entries - min_entries + 1;
            let entries = min_entries + (key(index, 0, 9)[0] as usize * 251 + index) % spread;
            sizes.push(entries);
            fs::write(
                root.join(format!("prototype-{index}.json")),
                prototype(index, entries),
            )
            .unwrap();
        }
        for view in 0..views {
            let store = ViewStore::open(&storage, &format!("bench-view-{view:04}")).unwrap();
            for generation in 0..generations {
                let source = root.join(format!(
                    "prototype-{}.json",
                    (view * generations + generation) % distinct
                ));
                let target = store
                    .manifest_path(&format!("generation-{generation:03}"))
                    .unwrap();
                fs::hard_link(source, target).unwrap();
            }
        }
        // A sample of live keys from every prototype, plus a large
        // unreferenced tail so each pass has deletion work in both planes.
        for (plane, label) in [(BlobPlane::Semantic, 0), (BlobPlane::Callgraph, 1)] {
            let referenced: Vec<[u8; 32]> = sizes
                .iter()
                .enumerate()
                .flat_map(|(index, entries)| {
                    (0..*entries)
                        .step_by(97)
                        .map(move |entry| key(index, entry, label))
                })
                .collect();
            seed_plane(&storage, plane, &referenced, 200_000);
        }
        let (manifests, json_bytes) = file_bytes(&storage, "json");
        println!(
            "fixture views={views} generations={generations} entries={min_entries}-{max_entries} distinct={distinct} manifests={manifests} manifest_mb={:.1} avg_manifest_kb={:.0} build_s={:.1}",
            json_bytes as f64 / 1e6,
            json_bytes as f64 / 1e3 / manifests.max(1) as f64,
            started.elapsed().as_secs_f64()
        );
        // Treat the manifests as existing generations: older than the window
        // inside which a sidecar derived from a manifest is not trusted.
        std::thread::sleep(std::time::Duration::from_secs(6));
    } else {
        println!("fixture reused (fresh process, sidecars already on disk)");
    }
    println!(
        "pass\twall_ms\tcpu_ms\tread_mb\talloc_mb\tallocs\theap_peak_mb\tpeak_rss_mb\texamined\tdeleted\tresult"
    );
    let own_view = storage.join("views").join("bench-view-0000");
    for pass in 1..=passes {
        peak_rss_reset();
        let wall = Instant::now();
        let cpu = cpu_seconds();
        let read = bytes_read();
        let total = TOTAL.load(Ordering::Relaxed);
        let count = COUNT.load(Ordering::Relaxed);
        let base = LIVE.load(Ordering::Relaxed);
        PEAK.store(base, Ordering::Relaxed);
        let result = sweep(SweepRequest {
            durable_storage: &storage,
            storage: &storage,
            family: "family",
            view_dir: &own_view,
            byte_budget: 2 * 1024 * 1024 * 1024,
            now_ms: BLOB_AGE_FLOOR_MS + 1,
            references: SweepReferences::default(),
        });
        let wall_ms = wall.elapsed().as_secs_f64() * 1e3;
        let cpu_ms = match (cpu, cpu_seconds()) {
            (Some(before), Some(after)) => format!("{:.0}", (after - before) * 1e3),
            _ => "n/a".into(),
        };
        let read_mb = match (read, bytes_read()) {
            (Some(before), Some(after)) => format!("{:.1}", (after - before) as f64 / 1e6),
            _ => "n/a".into(),
        };
        let alloc_mb = (TOTAL.load(Ordering::Relaxed) - total) as f64 / 1e6;
        let allocs = COUNT.load(Ordering::Relaxed) - count;
        let peak_mb = PEAK.load(Ordering::Relaxed).saturating_sub(base) as f64 / 1e6;
        let rss = peak_rss_mb();
        let (examined, deleted, outcome) = match result {
            Ok(report) => (report.examined_blobs, report.deleted_blobs, "ok".to_owned()),
            Err(error) => (0, 0, format!("err: {error}")),
        };
        println!(
            "{pass}\t{wall_ms:.0}\t{cpu_ms}\t{read_mb}\t{alloc_mb:.1}\t{allocs}\t{peak_mb:.1}\t{rss}\t{examined}\t{deleted}\t{outcome}"
        );
    }
    let (sidecars, sidecar_bytes) = file_bytes(&storage, "keys");
    println!(
        "sidecars={sidecars} sidecar_mb={:.1}",
        sidecar_bytes as f64 / 1e6
    );
}
