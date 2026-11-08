//! Tests of allocation tracing with the tracer installed as the global
//! allocator, which only the `aft` binary does.

use std::hint::black_box;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use aft::alloc_trace::{self, Label};

static LINES: Mutex<Vec<String>> = Mutex::new(Vec::new());
static ALLOCATE_IN_SINK: AtomicBool = AtomicBool::new(false);
static SINK_LARGE_ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

const MIB: usize = 1 << 20;

fn sink(line: &str) {
    LINES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(line.to_string());
    if ALLOCATE_IN_SINK.load(Ordering::SeqCst) {
        // A large allocation made while a line is written: tracing it would
        // record another event from inside the logger.
        black_box(Vec::<u8>::with_capacity(100 * MIB));
        SINK_LARGE_ALLOCATIONS.fetch_add(1, Ordering::SeqCst);
    }
}

fn lines_matching(needle: &str) -> Vec<String> {
    LINES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter(|line| line.contains(needle))
        .cloned()
        .collect()
}

fn wait_for_line(needle: &str) -> Vec<String> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        alloc_trace::drain_now();
        let found = lines_matching(needle);
        if !found.is_empty() || Instant::now() >= deadline {
            return found;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[inline(never)]
fn allocate_large_for_test() -> Vec<u8> {
    black_box(Vec::with_capacity(black_box(100 * MIB)))
}

/// The image name and offset of `address`, as the tracer prints frames.
fn image_offset(address: usize) -> (String, usize) {
    let mut info = std::mem::MaybeUninit::<libc::Dl_info>::zeroed();
    // SAFETY: dladdr fills `info` for an address in a loaded image.
    assert_ne!(
        unsafe { libc::dladdr(address as *const libc::c_void, info.as_mut_ptr()) },
        0
    );
    let info = unsafe { info.assume_init() };
    let path = unsafe { std::ffi::CStr::from_ptr(info.dli_fname) }.to_string_lossy();
    let image = path.rsplit('/').next().unwrap().to_string();
    (image, address - info.dli_fbase as usize)
}

/// Frames of a written line as (image, offset) pairs.
fn frames_of(line: &str) -> Vec<(String, usize)> {
    let frames = line.split(" frames=").nth(1).expect("line has frames");
    frames
        .split(',')
        .filter_map(|frame| {
            let frame = frame.split('[').next().unwrap();
            let (image, offset) = frame.rsplit_once("+0x")?;
            Some((image.to_string(), usize::from_str_radix(offset, 16).ok()?))
        })
        .collect()
}

#[test]
fn large_allocation_logs_one_event_with_its_label_and_caller() {
    alloc_trace::set_line_sink(sink);
    let label = Label::custom("test_large_allocation");
    let held = {
        let _scope = alloc_trace::scope(label, Some(std::path::Path::new("/work/large-repo")));
        allocate_large_for_test()
    };
    let lines = wait_for_line("op=test_large_allocation");
    drop(held);
    for line in &lines {
        eprintln!("{line}");
    }
    assert_eq!(lines.len(), 1, "{lines:#?}");
    let line = &lines[0];
    assert!(line.starts_with("large allocation 100 MiB alloc"), "{line}");
    assert!(line.contains(" root=large-repo"), "{line}");
    assert!(line.contains(" build_id="), "{line}");
    // One frame is the return address inside the function that allocated.
    let (image, start) = image_offset(allocate_large_for_test as usize);
    let frames = frames_of(line);
    assert!(
        frames
            .iter()
            .any(|(frame_image, offset)| *frame_image == image
                && (start..start + 512).contains(offset)),
        "no frame inside allocate_large_for_test ({image}+{start:#x}): {line}"
    );
    // The macOS loader knows a test binary's local symbols, so the line
    // names the function itself there; Linux's knows only exported ones.
    #[cfg(target_os = "macos")]
    assert!(
        line.contains("[aft::alloc_trace_process_tests::allocate_large_for_test]"),
        "{line}"
    );
}

#[test]
fn many_small_allocations_crossing_the_burst_threshold_log_once() {
    alloc_trace::set_line_sink(sink);
    let label = Label::custom("test_small_burst");
    {
        let _scope = alloc_trace::scope(label, Some(std::path::Path::new("/work/burst-repo")));
        // 640 MiB in 64 KiB pieces, none of them large on its own.
        let mut pieces = Vec::with_capacity(10_240);
        for _ in 0..10_240 {
            pieces.push(black_box(Vec::<u8>::with_capacity(64 * 1024)));
        }
        assert!(alloc_trace::live_bytes(label) >= 600 * MIB as isize);
        alloc_trace::drain_now();
        drop(pieces);
    }
    let crossed = wait_for_line("allocation burst: op=test_small_burst");
    assert_eq!(crossed.len(), 1, "{crossed:#?}");
    assert!(crossed[0].contains(" root=burst-repo"), "{}", crossed[0]);
    let ended = wait_for_line("allocation burst ended: op=test_small_burst");
    assert_eq!(ended.len(), 1, "{ended:#?}");
    assert!(ended[0].contains("peak 6"), "{}", ended[0]);
}

#[test]
fn label_carried_into_a_pool_task_is_attributed_to_it() {
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(1)
        .build()
        .unwrap();
    let carried = Label::custom("test_pool_carried");
    let uncarried = Label::custom("test_pool_uncarried");

    let _scope = alloc_trace::scope(carried, None);
    let carry = alloc_trace::carry();
    let held = pool.install(move || {
        let _label = carry.enter();
        black_box(Vec::<u8>::with_capacity(4 * MIB))
    });
    assert!(
        alloc_trace::live_bytes(carried) >= 4 * MIB as isize,
        "carried: {}",
        alloc_trace::live_bytes(carried)
    );
    drop(held);
    drop(_scope);

    let _scope = alloc_trace::scope(uncarried, None);
    let held = pool.install(|| black_box(Vec::<u8>::with_capacity(4 * MIB)));
    assert!(
        alloc_trace::live_bytes(uncarried) < MIB as isize,
        "uncarried: {}",
        alloc_trace::live_bytes(uncarried)
    );
    drop(held);
}

#[test]
fn the_census_names_the_operation_with_the_highest_mark() {
    alloc_trace::set_line_sink(sink);
    alloc_trace::start_logger();
    let label = Label::custom("test_census_mark");
    {
        let _scope = alloc_trace::scope(label, None);
        // Higher than any other test's label reaches, so it stays in the
        // census's top five while those run alongside.
        drop(black_box(Vec::<u8>::with_capacity(1 << 30)));
    }
    let census = aft::memory::MemorySnapshot::new_uncapped("ready", Default::default());
    assert!(census.allocation_labels.len() <= 5, "{census:?}");
    let mark = census
        .allocation_labels
        .iter()
        .find(|peak| peak.label == "test_census_mark")
        .unwrap_or_else(|| panic!("{:?}", census.allocation_labels));
    assert!(mark.peak_bytes >= 1 << 30, "{mark:?}");
    assert_eq!(mark.live_bytes, 0, "{mark:?}");
}

#[test]
fn the_loggers_own_allocations_are_never_traced() {
    alloc_trace::set_line_sink(sink);
    ALLOCATE_IN_SINK.store(true, Ordering::SeqCst);
    alloc_trace::start_logger();
    std::thread::Builder::new()
        .name("alloc-recurse".to_string())
        .spawn(|| {
            let _scope = alloc_trace::scope(Label::custom("test_logger_recursion"), None);
            drop(allocate_large_for_test());
        })
        .unwrap()
        .join()
        .unwrap();
    // The logger thread writes the line; its sink then allocates 100 MiB.
    let lines = wait_for_line("op=test_logger_recursion");
    assert_eq!(lines.len(), 1, "{lines:#?}");
    // A drain on another thread allocates in the sink too.
    std::thread::Builder::new()
        .name("alloc-drainer".to_string())
        .spawn(alloc_trace::drain_now)
        .unwrap()
        .join()
        .unwrap();
    alloc_trace::drain_now();
    assert!(SINK_LARGE_ALLOCATIONS.load(Ordering::SeqCst) > 0);
    let traced_in_logger = lines_matching("thread=aft-alloc-trace");
    assert!(traced_in_logger.is_empty(), "{traced_in_logger:#?}");
    let traced_in_drainer = lines_matching("thread=alloc-drainer");
    assert!(traced_in_drainer.is_empty(), "{traced_in_drainer:#?}");
}
