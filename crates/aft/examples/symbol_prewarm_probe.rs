//! Measures the symbol-cache prewarm a bind runs, for a repository's main
//! checkout and for new worktrees whose files match it: the prewarm's wall
//! and CPU time, the bytes it allocates, the most it holds at once beyond
//! what was held before, what it leaves held, and where its time goes per
//! file; then three matching worktrees prewarmed at once, with the highest
//! footprint the process reaches. All checkouts are built into one index
//! store, as every worktree of a repository shares one, so the worktrees'
//! builds reuse the main checkout's records. Each line of stdout is one
//! measurement, as JSON.
//!
//! ```text
//! cargo run --release -p agent-file-tools --example symbol_prewarm_probe -- \
//!     <repo or archive.tar.gz> <rev> [threads] [scratch]
//! ```
//!
//! The checkouts are exported with `git archive` into `scratch` (a new
//! temporary folder under `TMPDIR` when not given), so the repository
//! itself is not touched. The main checkout keeps the commit's
//! modification times; the worktree's files are written now, as a new
//! worktree's are.

#![allow(clippy::unwrap_used, clippy::print_stdout, clippy::print_stderr)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use aft::data_layer::checkout::{Checkout, DataLayer};
use aft::data_layer::symbol_prewarm_measure as measure;
use aft_store::ids::{Digest128, FamilyId};
use serde_json::{json, Value};

/// Bytes held by live allocations.
static LIVE: AtomicI64 = AtomicI64::new(0);
/// The most [`LIVE`] reached since the last [`Allocations::start`].
static PEAK: AtomicI64 = AtomicI64::new(0);
/// Bytes asked for since the process started.
static ALLOCATED: AtomicU64 = AtomicU64::new(0);

/// The system allocator, counting what each call asks for and gives back.
struct Counting;

impl Counting {
    fn grow(size: usize) {
        let live = LIVE.fetch_add(size as i64, Ordering::Relaxed) + size as i64;
        PEAK.fetch_max(live, Ordering::Relaxed);
        ALLOCATED.fetch_add(size as u64, Ordering::Relaxed);
    }

    fn shrink(size: usize) {
        LIVE.fetch_sub(size as i64, Ordering::Relaxed);
    }
}

// SAFETY: every call is passed to the system allocator unchanged.
unsafe impl std::alloc::GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        Self::grow(layout.size());
        // SAFETY: the caller's contract is the system allocator's.
        unsafe { std::alloc::System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: std::alloc::Layout) -> *mut u8 {
        Self::grow(layout.size());
        // SAFETY: as above.
        unsafe { std::alloc::System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: std::alloc::Layout, new_size: usize) -> *mut u8 {
        if new_size >= layout.size() {
            Self::grow(new_size - layout.size());
        } else {
            Self::shrink(layout.size() - new_size);
        }
        // SAFETY: as above.
        unsafe { std::alloc::System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
        Self::shrink(layout.size());
        // SAFETY: as above.
        unsafe { std::alloc::System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// The allocations made between [`Allocations::start`] and
/// [`Allocations::report`].
struct Allocations {
    live: i64,
    allocated: u64,
    cpu: f64,
    started: Instant,
}

impl Allocations {
    fn start() -> Self {
        let live = LIVE.load(Ordering::Relaxed);
        PEAK.store(live, Ordering::Relaxed);
        Self {
            live,
            allocated: ALLOCATED.load(Ordering::Relaxed),
            cpu: cpu_seconds(),
            started: Instant::now(),
        }
    }

    fn report(&self) -> Value {
        let mib = |bytes: i64| (bytes as f64 / 1048576.0 * 10.0).round() / 10.0;
        json!({
            "wall_ms": (self.started.elapsed().as_secs_f64() * 10_000.0).round() / 10.0,
            "cpu_ms": ((cpu_seconds() - self.cpu) * 10_000.0).round() / 10.0,
            "allocated_mib": mib((ALLOCATED.load(Ordering::Relaxed) - self.allocated) as i64),
            "peak_added_mib": mib(PEAK.load(Ordering::Relaxed) - self.live),
            "retained_mib": mib(LIVE.load(Ordering::Relaxed) - self.live),
        })
    }
}

/// User and system CPU seconds this process has spent, its threads included.
#[cfg(unix)]
fn cpu_seconds() -> f64 {
    // SAFETY: getrusage only fills the struct it is given.
    let usage = unsafe {
        let mut usage = std::mem::zeroed::<libc::rusage>();
        libc::getrusage(libc::RUSAGE_SELF, &raw mut usage);
        usage
    };
    let seconds = |time: libc::timeval| time.tv_sec as f64 + f64::from(time.tv_usec as i32) / 1e6;
    seconds(usage.ru_utime) + seconds(usage.ru_stime)
}

/// Windows has no getrusage; the probe reports 0 there and wall time only.
#[cfg(not(unix))]
fn cpu_seconds() -> f64 {
    0.0
}

/// Writes the tree of `rev` in `repo` into `dir`; with `now`, every file's
/// modification time is the moment it is written rather than the commit's.
/// A `repo` ending in `.tar.gz` is an archive `git archive` already wrote,
/// for a machine without the repository; `rev` is then ignored.
fn export(repo: &Path, rev: &str, dir: &Path, now: bool) {
    std::fs::create_dir_all(dir).unwrap();
    if repo.to_string_lossy().ends_with(".tar.gz") {
        let mut tar = Command::new("tar");
        tar.arg("-xz");
        if now {
            tar.arg("-m");
        }
        let status = tar.arg("-f").arg(repo).arg("-C").arg(dir).status().unwrap();
        assert!(status.success());
        return;
    }
    let mut archive = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["archive", rev])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut tar = Command::new("tar");
    tar.arg("-x");
    if now {
        tar.arg("-m");
    }
    let status = tar
        .arg("-C")
        .arg(dir)
        .stdin(archive.stdout.take().unwrap())
        .status()
        .unwrap();
    assert!(status.success() && archive.wait().unwrap().success());
}

fn bind(layer: &Arc<DataLayer>, store: &Path, family: FamilyId, root: &Path) -> Checkout {
    let checkout = Checkout::bind(Arc::clone(layer), store, family, root).unwrap();
    let measured = Allocations::start();
    let report = checkout.build().unwrap();
    println!(
        "{}",
        json!({
            "step": "build",
            "root": root.file_name().unwrap().to_string_lossy(),
            "cost": measured.report(),
            "report": format!("{report:?}"),
        })
    );
    checkout
}

/// Runs the bind's prewarm for `checkout` into a cache of its own, which
/// is returned so it stays held while later checkouts are measured.
fn prewarm(label: &str, checkout: &Checkout) -> aft::parser::SharedSymbolCache {
    let view = checkout.view().unwrap();
    let cache = measure::fresh_cache();
    let measured = Allocations::start();
    let report = measure::prewarm_into(&view, &cache).unwrap();
    let cost = measured.report();
    println!(
        "{}",
        json!({ "step": "prewarm", "checkout": label, "cost": cost, "report": report })
    );
    let measured = Allocations::start();
    let phases = measure::phases(&view).unwrap();
    println!(
        "{}",
        json!({ "step": "phases", "checkout": label, "cost": measured.report(), "phases": phases })
    );
    cache
}

/// The process's physical footprint (macOS) or resident set (Linux) in
/// bytes; 0 elsewhere.
#[cfg(target_os = "macos")]
fn footprint() -> u64 {
    let mut usage = std::mem::MaybeUninit::<libc::rusage_info_v4>::zeroed();
    // SAFETY: proc_pid_rusage only fills the struct it is given.
    let rc = unsafe {
        libc::proc_pid_rusage(
            libc::getpid(),
            libc::RUSAGE_INFO_V4,
            usage.as_mut_ptr().cast(),
        )
    };
    if rc != 0 {
        return 0;
    }
    // SAFETY: filled by the successful call above.
    unsafe { usage.assume_init() }.ri_phys_footprint
}

/// The process's physical footprint (macOS) or resident set (Linux) in
/// bytes; 0 elsewhere.
#[cfg(not(target_os = "macos"))]
fn footprint() -> u64 {
    std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|statm| statm.split_whitespace().nth(1)?.parse::<u64>().ok())
        .map_or(0, |pages| pages * 4096)
}

/// Runs the bind's prewarm for every checkout of `worktrees` at once, each
/// on a thread and into a cache of its own, as when several worktrees bind
/// together, and reports the wall time, the allocations and the highest
/// footprint the process reached, sampled every 5 ms.
fn prewarm_together(worktrees: &[Checkout]) -> Vec<aft::parser::SharedSymbolCache> {
    let views = worktrees
        .iter()
        .map(|checkout| checkout.view().unwrap())
        .collect::<Vec<_>>();
    let caches = views
        .iter()
        .map(|_| measure::fresh_cache())
        .collect::<Vec<_>>();
    let before = footprint();
    let peak = Arc::new(AtomicU64::new(before));
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let sampler = {
        let (peak, done) = (Arc::clone(&peak), Arc::clone(&done));
        std::thread::spawn(move || {
            while !done.load(Ordering::Acquire) {
                peak.fetch_max(footprint(), Ordering::AcqRel);
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        })
    };
    let measured = Allocations::start();
    let reports = std::thread::scope(|scope| {
        let running = views
            .iter()
            .zip(&caches)
            .map(|(view, cache)| scope.spawn(move || measure::prewarm_into(view, cache).unwrap()))
            .collect::<Vec<_>>();
        running
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>()
    });
    let cost = measured.report();
    done.store(true, Ordering::Release);
    sampler.join().unwrap();
    let mib = |bytes: u64| (bytes as f64 / 1048576.0 * 10.0).round() / 10.0;
    println!(
        "{}",
        json!({
            "step": "prewarm together",
            "checkouts": worktrees.len(),
            "cost": cost,
            "footprint_before_mib": mib(before),
            "footprint_peak_added_mib": mib(peak.load(Ordering::Acquire).saturating_sub(before)),
            "reports": reports,
        })
    );
    caches
}

fn main() {
    let mut args = std::env::args().skip(1);
    let repo = PathBuf::from(args.next().unwrap());
    let rev = args.next().unwrap();
    let threads = args.next().map_or(8, |threads| threads.parse().unwrap());
    let kept;
    let scratch = match args.next() {
        Some(scratch) => PathBuf::from(scratch),
        None => {
            kept = tempfile::tempdir().unwrap();
            kept.path().to_path_buf()
        }
    };
    let store = scratch.join("store");
    let main_root = scratch.join("main");
    export(&repo, &rev, &main_root, false);
    let main_root = std::fs::canonicalize(main_root).unwrap();
    // Four matching worktrees: one bound alone, three bound together.
    let worktree_roots = (0..4)
        .map(|index| {
            let root = scratch.join(format!("worktree-{index}"));
            export(&repo, &rev, &root, true);
            std::fs::canonicalize(root).unwrap()
        })
        .collect::<Vec<_>>();
    let layer = Arc::new(DataLayer::new(256 << 20, threads));
    let family = FamilyId::from_digest(Digest128::of(b"symbol prewarm probe"));

    let main = bind(&layer, &store, family, &main_root);
    let worktrees = worktree_roots
        .iter()
        .map(|root| bind(&layer, &store, family, root))
        .collect::<Vec<_>>();
    // The main checkout's prewarm after a restart: its records are stored,
    // none decoded yet in this process.
    let main_cache = prewarm("main", &main);
    // A new worktree bound while the main checkout's cache is held.
    let worktree_cache = prewarm("worktree", &worktrees[0]);
    // The same worktree bound again with a cache of its own, as a later
    // bind after an idle eviction is.
    let again = prewarm("worktree again", &worktrees[0]);
    // Three new worktrees bound at once.
    let together = prewarm_together(&worktrees[1..]);
    drop((main_cache, worktree_cache, again, together));
}
