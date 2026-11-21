//! Publication holder equivalence and an opt-in, real-source scaling probe.

use super::*;
use std::cell::{Cell, RefCell};
use std::time::Duration;

thread_local! {
    static REUSE: Cell<bool> = const { Cell::new(true) };
    static HOLDER_PROBES: Cell<usize> = const { Cell::new(0) };
    static TIMINGS: RefCell<Option<Vec<(&'static str, f64, f64)>>> = const { RefCell::new(None) };
    static STREAM_DELTA: Cell<bool> = const { Cell::new(true) };
    static DELTA_SPILL_FAIL: Cell<bool> = const { Cell::new(false) };
    static DELTA_STAGE: RefCell<Option<Box<dyn FnMut(&str)>>> = const { RefCell::new(None) };
}

pub(super) fn stream_delta() -> bool {
    STREAM_DELTA.with(Cell::get)
}

pub(super) fn at_delta_stage(stage: &str) {
    DELTA_STAGE.with_borrow_mut(|hook| {
        if let Some(hook) = hook {
            hook(stage);
        }
    });
}

pub(super) fn delta_spill_check() -> std::io::Result<()> {
    if DELTA_SPILL_FAIL.with(|fail| fail.replace(false)) {
        return Err(std::io::Error::other("injected delta spill write error"));
    }
    Ok(())
}

fn with_delta_stage<T>(hook: impl FnMut(&str) + 'static, f: impl FnOnce() -> T) -> T {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            DELTA_STAGE.with_borrow_mut(|hook| *hook = None);
        }
    }
    DELTA_STAGE.with_borrow_mut(|slot| *slot = Some(Box::new(hook)));
    let _reset = Reset;
    f()
}

pub(super) fn reuse_lookups() -> bool {
    REUSE.with(Cell::get)
}

pub(super) fn note_holder_probe() {
    HOLDER_PROBES.with(|probes| probes.set(probes.get() + 1));
}

fn with_reuse<T>(reuse: bool, f: impl FnOnce() -> T) -> T {
    struct Reset(bool);
    impl Drop for Reset {
        fn drop(&mut self) {
            REUSE.with(|value| value.set(self.0));
        }
    }
    let _reset = Reset(REUSE.with(|value| value.replace(reuse)));
    f()
}

pub(in crate::data_layer) struct Phase {
    name: &'static str,
    start: Option<(Instant, Duration)>,
}

pub(in crate::data_layer) fn phase(name: &'static str) -> Phase {
    Phase {
        name,
        start: TIMINGS.with(|timings| timings.borrow().as_ref().map(|_| (Instant::now(), cpu()))),
    }
}

pub(super) fn next_phase(previous: &mut Phase, name: &'static str) {
    // Finish the previous phase before starting the next clock.
    previous.finish();
    *previous = phase(name);
}

impl Phase {
    fn finish(&mut self) {
        if let Some((wall, start_cpu)) = self.start.take() {
            let elapsed = wall.elapsed().as_secs_f64() * 1000.0;
            let elapsed_cpu = cpu().saturating_sub(start_cpu).as_secs_f64() * 1000.0;
            TIMINGS.with(|timings| {
                if let Some(timings) = timings.borrow_mut().as_mut() {
                    timings.push((self.name, elapsed, elapsed_cpu));
                }
            });
        }
    }
}

impl Drop for Phase {
    fn drop(&mut self) {
        self.finish();
    }
}

#[cfg(target_os = "linux")]
fn cpu() -> Duration {
    let mut now = std::mem::MaybeUninit::<libc::timespec>::uninit();
    // SAFETY: clock_gettime initializes the supplied timespec on success.
    assert_eq!(
        unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, now.as_mut_ptr()) },
        0
    );
    let now = unsafe { now.assume_init() };
    Duration::new(now.tv_sec as u64, now.tv_nsec as u32)
}

#[cfg(not(target_os = "linux"))]
fn cpu() -> Duration {
    Duration::ZERO
}

fn write(root: &Path, path: &str, text: &str) {
    let file = root.join(path);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(file, text).unwrap();
}

fn family() -> FamilyId {
    FamilyId::from_digest(Digest128::of(b"publication-fixture"))
}

fn scratch() -> tempfile::TempDir {
    let base = crate::test_support::manifest_dir().join("../../target/scratch/publication");
    std::fs::create_dir_all(&base).unwrap();
    tempfile::tempdir_in(base).unwrap()
}

fn delta_fixture(dir: &Path) -> Arc<Checkout> {
    let root = dir.join("root");
    write(&root, "a.ts", "export function a() { return 1; }");
    write(&root, "b.ts", "export function b() { return 2; }");
    write(&root, "notes.txt", &"x".repeat(1 << 20));
    let checkout = Arc::new(
        Checkout::bind(
            Arc::new(DataLayer::new(8 << 20, 2)),
            &dir.join("store"),
            family(),
            &root,
        )
        .unwrap(),
    );
    checkout.build().unwrap();
    checkout
}

fn edit_delta(checkout: &Checkout, path: &str, text: &str) {
    write(checkout.root(), path, text);
    checkout.record_paths(&[path.as_bytes().to_vec()]).unwrap();
}

fn refresh_delta(checkout: &Checkout) {
    edit_delta(checkout, "a.ts", "export function a() { return 3; }");
    edit_delta(checkout, "b.ts", "export function b() { return 4; }");
    assert!(!checkout.refresh_callgraph().unwrap().unwrap().cold_build);
}

fn latest_segment(checkout: &Checkout) -> Arc<Segment> {
    let snapshot = checkout.family.snapshot().unwrap();
    let id = snapshot.manifest().segments().last().unwrap().id;
    snapshot
        .open_shared_segment(id, &known_sections(), &checkout.layer.cache)
        .unwrap()
}

fn sections(
    segment: &Segment,
    keep: impl Fn(aft_store::segment::SectionKind) -> bool,
) -> BTreeMap<aft_store::segment::SectionId, (aft_store::segment::SectionInfo, Vec<u8>)> {
    segment
        .sections()
        .filter(|(_, info)| keep(info.id.kind))
        .map(|(section, info)| {
            (
                info.id,
                (info, segment.read_range(section, 0..info.len).unwrap()),
            )
        })
        .collect()
}

fn assert_no_delta_scratch(checkout: &Checkout) {
    let builds = checkout
        .family
        .dir()
        .join("builds")
        .join(checkout.checkout.to_string());
    assert!(std::fs::read_dir(builds)
        .unwrap()
        .all(|entry| !entry.unwrap().path().is_dir()));
    assert!(
        checkout.family.try_write().unwrap().is_some(),
        "writer session still held"
    );
}

#[test]
fn delta_publication_streams_byte_identical_sections() {
    let raw = |segment: &Segment| {
        let mut bytes = std::fs::read(segment.path()).unwrap();
        // Each writer session has its own identifier and creation timestamp.
        // Normalize those fields and their header checksum, nothing else.
        bytes[40..56].fill(0);
        bytes[112..128].fill(0);
        bytes
    };
    let run = |streamed| {
        let dir = scratch();
        let checkout = delta_fixture(dir.path());
        refresh_delta(&checkout);
        STREAM_DELTA.with(|value| value.set(streamed));
        let report = checkout.publish_changes();
        STREAM_DELTA.with(|value| value.set(true));
        assert_eq!(report.unwrap().unwrap().fragments, 2);
        assert_no_delta_scratch(&checkout);
        let latest = latest_segment(&checkout);
        (raw(&latest), sections(&latest, |_| true))
    };
    let (buffered, buffered_sections) = run(false);
    let (streamed, streamed_sections) = run(true);
    assert_eq!(streamed_sections, buffered_sections);
    assert!(streamed == buffered, "delta segment bytes differ");
}

fn publication_crossing(stage: &'static str) {
    use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

    let dir = scratch();
    let checkout = delta_fixture(dir.path());
    refresh_delta(&checkout);
    let old = checkout.family.snapshot().unwrap();
    let before = source_work_for_test(checkout.root());
    let counts = checkout.refresh_counts();
    let crossed = Arc::new(AtomicBool::new(false));
    let sample = Arc::clone(&crossed);
    let guard = Arc::new(RefreshMemory::simulated(1, move |_, _| {
        Some(if sample.load(Relaxed) { 2 } else { 0 })
    }));
    let mut added = 0;
    assert!(with_delta_stage(
        move |here| {
            assert_ne!(
                here, "before_segment",
                "spill did not stop before finish_segment"
            );
            added += usize::from(here == "added");
            assert!(
                stage != "added" || added <= 1,
                "a second fragment was added after crossing"
            );
            if here == stage {
                assert_ne!(added, 0, "crossed before any fragment was added");
                crossed.store(true, Relaxed);
            }
        },
        || checkout.publish_changes_with(false, guard, Instant::now())
    )
    .unwrap()
    .is_none());
    assert_no_delta_scratch(&checkout);
    assert!(checkout.engine.lock().unwrap().is_none());
    assert_eq!(
        checkout.refresh_counts().guard_fallbacks,
        counts.guard_fallbacks + 1
    );
    assert_eq!(
        source_work_for_test(checkout.root()).cold_builds,
        before.cold_builds + 1
    );
    let newest = checkout.family.snapshot().unwrap();
    assert_eq!(
        newest.manifest().generation().get(),
        old.manifest().generation().get() + 1,
        "only the cold build may commit"
    );
    assert_eq!(newest.manifest().segments().len(), 1);

    let cold_dir = scratch();
    let cold = Checkout::bind(
        Arc::clone(&checkout.layer),
        cold_dir.path(),
        family(),
        checkout.root(),
    )
    .unwrap();
    cold.build().unwrap();
    let is_graph = |kind| {
        aft_store::graph::KNOWN
            .iter()
            .chain(&aft_store::graph::KNOWN_OPTIONAL)
            .any(|section| section.kind == kind)
    };
    assert_eq!(
        sections(&latest_segment(&checkout), is_graph),
        sections(&latest_segment(&cold), is_graph)
    );
}

#[test]
fn delta_publication_guard_crosses_after_add() {
    publication_crossing("added");
}

#[test]
fn delta_publication_guard_crosses_inside_finish() {
    publication_crossing("finish");
}

#[test]
fn delta_publication_guard_stands_down_for_reserved_build() {
    let dir = scratch();
    let checkout = delta_fixture(dir.path());
    refresh_delta(&checkout);
    let before = source_work_for_test(checkout.root());
    let counts = checkout.refresh_counts();
    let old_generation = checkout.family.snapshot().unwrap().manifest().generation();
    let reserved = Arc::new(Mutex::new(None));
    let (holder, slot) = (Arc::clone(&checkout), Arc::clone(&reserved));
    let guard = Arc::new(RefreshMemory::simulated(1, |files, _| {
        Some(if files > 0 { 2 } else { 0 })
    }));
    assert!(with_delta_stage(
        move |here| {
            if here == "entry" {
                *slot.lock().unwrap() = Some(holder.reserve_build().unwrap());
                assert!(
                    holder.publish_changes().unwrap().is_none(),
                    "reservation blocks entry"
                );
            }
        },
        || checkout.publish_changes_with(false, guard, Instant::now())
    )
    .unwrap()
    .is_none());
    assert_no_delta_scratch(&checkout);
    assert!(checkout.engine.lock().unwrap().is_none());
    assert_eq!(
        checkout.refresh_counts().guard_fallbacks,
        counts.guard_fallbacks + 1
    );
    assert_eq!(source_work_for_test(checkout.root()), before);
    assert_eq!(
        checkout.family.snapshot().unwrap().manifest().generation(),
        old_generation
    );
    assert_eq!(checkout.live.read().unwrap().changes.len(), 2);

    edit_delta(&checkout, "before.ts", "export function before() {}");
    let publication = hold_build_publication_for_test(checkout.root());
    let reservation = reserved.lock().unwrap().take().unwrap();
    let holder = Arc::clone(&checkout);
    let build = std::thread::spawn(move || holder.build_reserved(false, reservation).unwrap());
    publication.wait_until_held();
    edit_delta(&checkout, "after.ts", "export function after() {}");
    drop(publication);
    build.join().unwrap();
    let published = checkout
        .family
        .snapshot()
        .unwrap()
        .checkout_state(checkout.checkout)
        .unwrap()
        .unwrap();
    assert!(published.get(b"before.ts").is_some());
    assert!(published.get(b"after.ts").is_none());
    assert_eq!(
        checkout
            .live
            .read()
            .unwrap()
            .changes
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        [b"after.ts".to_vec()]
    );
    assert!(!checkout.refresh_callgraph().unwrap().unwrap().cold_build);
    assert_eq!(checkout.publish_changes().unwrap().unwrap().paths, 1);
    let published = checkout
        .family
        .snapshot()
        .unwrap()
        .checkout_state(checkout.checkout)
        .unwrap()
        .unwrap();
    assert!(published.get(b"after.ts").is_some());
    assert!(checkout.live.read().unwrap().changes.is_empty());
}

#[test]
fn delta_publication_spill_io_and_cancellation_do_not_commit() {
    let dir = scratch();
    let checkout = delta_fixture(dir.path());
    refresh_delta(&checkout);
    let old_generation = checkout.family.snapshot().unwrap().manifest().generation();
    let counts = checkout.refresh_counts();
    let before = source_work_for_test(checkout.root());
    for cancel in [false, true] {
        let token = crate::executor::JobCancellation::new();
        let _context = crate::executor::install_job_cancellation(token.clone());
        let error = with_delta_stage(
            move |here| {
                assert_ne!(
                    here, "before_segment",
                    "spill did not stop before finish_segment"
                );
                if here == "finish" {
                    if cancel {
                        token.request_cancel();
                    } else {
                        DELTA_SPILL_FAIL.with(|fail| fail.set(true));
                    }
                }
            },
            || checkout.publish_changes(),
        )
        .unwrap_err();
        if cancel {
            assert!(matches!(error, DataError::Cancelled), "{error:?}");
        } else {
            assert!(
                matches!(error, DataError::Store(message) if message.contains("injected delta spill write error"))
            );
        }
        assert_no_delta_scratch(&checkout);
        assert_eq!(
            checkout.family.snapshot().unwrap().manifest().generation(),
            old_generation
        );
        assert_eq!(checkout.refresh_counts(), counts);
        assert_eq!(source_work_for_test(checkout.root()), before);
        assert_eq!(checkout.live.read().unwrap().changes.len(), 2);
    }
    assert!(
        checkout.publish_changes().unwrap().is_some(),
        "the same edit can retry"
    );
}

#[test]
fn delta_publication_admits_only_the_delta_estimate_and_keeps_disk_floor() {
    let dir = scratch();
    let checkout = delta_fixture(dir.path());
    refresh_delta(&checkout);
    let capacity = 1 << 40;
    let floor = crate::disk_floor::free_disk_floor(capacity);
    let old_generation = checkout.family.snapshot().unwrap().manifest().generation();
    for free in [floor - 1, floor] {
        let _free =
            crate::disk_floor::set_free_space_for_test(checkout.family.dir(), free, capacity);
        assert!(matches!(
            checkout.publish_changes(),
            Err(DataError::DiskFloor(_))
        ));
        assert_no_delta_scratch(&checkout);
        assert_eq!(
            checkout.family.snapshot().unwrap().manifest().generation(),
            old_generation
        );
    }
    let whole = checkout
        .view()
        .unwrap()
        .state()
        .get(b"notes.txt")
        .unwrap()
        .stat
        .size
        * 8;
    let headroom = 512 << 10;
    assert!(headroom < whole, "the whole-checkout estimate must not fit");
    let _free = crate::disk_floor::set_free_space_for_test(
        checkout.family.dir(),
        floor + headroom,
        capacity,
    );
    assert_eq!(checkout.publish_changes().unwrap().unwrap().fragments, 2);
    assert_no_delta_scratch(&checkout);
}

fn pin_readers(checkout: &Checkout, snapshot: &Snapshot, states: &[&CheckoutState]) {
    let collect = |reuse| {
        with_reuse(reuse, || {
            HOLDER_PROBES.with(|probes| probes.set(0));
            let mut notes = Vec::new();
            let read = checkout
                .segments_read_noting(snapshot, states, |member, unit, holder| {
                    notes.push(format!("{member}:{unit:?}:{holder}"));
                })
                .unwrap();
            ((read.needed, notes), HOLDER_PROBES.with(Cell::get))
        })
    };
    let (cached, cached_probes) = collect(true);
    let (original, original_probes) = collect(false);
    assert_eq!(cached, original, "newest holders and every member callback");
    assert!(cached_probes < original_probes, "shared units must not probe each sibling's tables again: {cached_probes} vs {original_probes}");
}

fn pin_retirement(checkout: &Checkout, own: &CheckoutState) {
    for own_reads in [false, true] {
        let collect = |reuse| {
            with_reuse(reuse, || {
                let retired = checkout.unneeded(own, own_reads, &[]).unwrap();
                (retired.segments, retired.checkouts)
            })
        };
        assert_eq!(
            collect(true),
            collect(false),
            "retirement, own_reads={own_reads}"
        );
    }
}

#[test]
fn publication_holder_memo_preserves_retirement_and_member_holders() {
    let dir = scratch();
    let store = dir.path().join("store");
    let layer = Arc::new(DataLayer::new(8 << 20, 2));
    let mut checkouts = Vec::new();
    for (index, text) in ["shared", "shared", "partly shared", "unshared"]
        .iter()
        .enumerate()
    {
        let root = dir.path().join(format!("checkout-{index}"));
        write(&root, "Cargo.toml", "[package]\nname = \"demo\"\n");
        write(
            &root,
            "src/lib.rs",
            "mod util; pub fn caller() { util::callee(); }\n",
        );
        write(&root, "src/util.rs", "pub fn callee() {}\n");
        write(&root, "notes.txt", text);
        if index == 3 {
            write(&root, "src/util.rs", "pub fn separate() {}\n");
            write(
                &root,
                "src/lib.rs",
                "mod util; pub fn alone() { util::separate(); }\n",
            );
        }
        let checkout = Checkout::bind(Arc::clone(&layer), &store, family(), &root).unwrap();
        checkout.build().unwrap();
        checkouts.push(checkout);
    }
    let checkout = &checkouts[0];
    for round in 0..4 {
        if round == 2 {
            // A sibling disappears after a previous holder computation. No
            // answer may survive into a later snapshot or hide its retirement.
            std::fs::remove_dir_all(checkouts[3].root()).unwrap();
        }
        let snapshot = checkout.family.snapshot().unwrap();
        let states = snapshot
            .manifest()
            .checkouts()
            .iter()
            .map(|entry| snapshot.checkout_state(entry.checkout).unwrap().unwrap())
            .collect::<Vec<_>>();
        pin_readers(checkout, &snapshot, &states.iter().collect::<Vec<_>>());
        // Repeated missing keys are memoized and still produce no holder.
        let mut missing_paths = states[0].paths().to_vec();
        missing_paths[0].content = ContentKey::from_digest(Digest128::of(b"absent content"));
        missing_paths[0].language = "rust".into();
        let fragments = vec![Some(Digest128::of(b"absent fragment")); missing_paths.len()];
        let missing = CheckoutState::new(Vec::new(), Digest128::of(b"missing"), missing_paths)
            .unwrap()
            .with_fragments_for(fragments, EXTRACTOR_VERSION)
            .unwrap();
        pin_readers(checkout, &snapshot, &[&missing, &missing]);
        let own = checkout.view().unwrap().state().clone();
        pin_retirement(checkout, &own);
        if round == 2 {
            assert!(checkout
                .unneeded(&own, true, &[])
                .unwrap()
                .checkouts
                .contains(&checkouts[3].checkout));
        }
        write(
            checkout.root(),
            "src/util.rs",
            &format!("pub fn callee() {{}}\npub fn round_{round}() {{ callee(); }}\n"),
        );
        checkout.record_paths(&[b"src/util.rs".to_vec()]).unwrap();
        checkout.refresh_callgraph().unwrap();
        checkout.publish_changes().unwrap().unwrap();
        pin_retirement(checkout, checkout.view().unwrap().state());
    }
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        let destination = to.join(entry.file_name());
        if path.is_dir() {
            copy_tree(&path, &destination);
        } else {
            std::fs::copy(path, destination).unwrap();
        }
    }
}

/// Run with --profile ci-storm --ignored --exact and --nocapture. Copies Git-tracked
/// AFT source once, then copies its persisted family for each paired run, so
/// both algorithms start from identical segment and checkout-state bytes.
#[test]
#[ignore = "release scaling measurement; builds a complete AFT source fixture"]
fn publication_family_scaling() {
    assert!(cfg!(target_os = "linux"), "CPU measurements require Linux");
    assert!(!cfg!(debug_assertions), "use a release build");
    let source = crate::test_support::manifest_dir().join("../..");
    let tracked = std::process::Command::new("git")
        .args(["ls-files", "-z"])
        .current_dir(&source)
        .output()
        .unwrap();
    assert!(tracked.status.success());
    let dir = scratch();
    let root = dir.path().join("source");
    let mut edits = Vec::new();
    let mut seed_edits = Vec::new();
    for path in tracked
        .stdout
        .split(|&byte| byte == 0)
        .filter(|path| !path.is_empty())
    {
        let relative = Path::new(std::str::from_utf8(path).unwrap());
        let original = source.join(relative);
        if !original.is_file() {
            continue;
        }
        let destination = root.join(relative);
        std::fs::create_dir_all(destination.parent().unwrap()).unwrap();
        std::fs::copy(&original, &destination).unwrap();
        if relative.starts_with("crates/aft/src")
            && relative.extension().is_some_and(|ext| ext == "rs")
        {
            if edits.len() < 50 {
                edits.push((path.to_vec(), std::fs::read(&original).unwrap()));
            } else if seed_edits.len() < 11 {
                seed_edits.push(path.to_vec());
            }
        }
    }
    assert_eq!(edits.len(), 50);
    // Ignore the source repository's artifacts just as a normal checkout does.
    let store = dir.path().join("seed-store");
    let layer = Arc::new(DataLayer::new(64 << 20, 8));
    let seed = Checkout::bind(Arc::clone(&layer), &store, family(), &root).unwrap();
    eprintln!("publication fixture: building AFT source");
    let built = seed.build().unwrap();
    assert!(
        seed.view().unwrap().state().paths().len() >= 6000,
        "{built:?}"
    );
    // Disjoint small publications keep their segments live, matching a
    // worktree that reads a base plus several incremental generations.
    for path in seed_edits {
        let file = root.join(std::str::from_utf8(&path).unwrap());
        let mut bytes = std::fs::read(&file).unwrap();
        bytes.extend_from_slice(b"\n// distinct incremental publication\n");
        std::fs::write(file, bytes).unwrap();
        seed.record_paths(&[path]).unwrap();
        seed.refresh_callgraph().unwrap();
        seed.publish_changes().unwrap().unwrap();
    }
    let state = seed.view().unwrap().state().clone();
    let segments = seed.family.snapshot().unwrap().manifest().segments().len();
    assert!(
        segments >= 12,
        "expected a base plus incremental segments, got {segments}"
    );
    eprintln!(
        "publication fixture: {} paths, {} parsed, {} bytes, {segments} segments; profile=ci-storm",
        state.paths().len(),
        built.parsed_files,
        built.segment_bytes
    );
    for n in [1, 10, 40] {
        let mut session = seed.writer().unwrap();
        let mut states = Vec::new();
        for sibling in 1..n {
            let sibling_root = dir.path().join(format!("sibling-{sibling}"));
            std::fs::create_dir_all(&sibling_root).unwrap();
            let id =
                CheckoutId::from_digest(Digest128::of(sibling_root.to_str().unwrap().as_bytes()));
            states.push(
                session
                    .write_checkout_state(id, &state.clone().with_root(&sibling_root))
                    .unwrap(),
            );
        }
        session
            .publish(&Publication {
                states,
                ..Publication::default()
            })
            .unwrap();
        drop(session);
        // ABBA order separates the algorithm from first-run cache effects.
        for (run, reuse) in [false, true, true, false].into_iter().enumerate() {
            for (path, original) in &edits {
                std::fs::write(root.join(std::str::from_utf8(path).unwrap()), original).unwrap();
            }
            let run_store = dir.path().join(format!("run-{n}-{run}"));
            copy_tree(&store, &run_store);
            let run_layer = Arc::new(DataLayer::new(64 << 20, 8));
            let checkout = Checkout::bind(run_layer, &run_store, family(), &root).unwrap();
            for (path, original) in &edits {
                let mut changed = original.clone();
                changed.extend_from_slice(b"\n// publication scaling fixture changed content\n");
                std::fs::write(root.join(std::str::from_utf8(path).unwrap()), changed).unwrap();
            }
            checkout
                .record_paths(
                    &edits
                        .iter()
                        .map(|(path, _)| path.clone())
                        .collect::<Vec<_>>(),
                )
                .unwrap();
            checkout.refresh_callgraph().unwrap();
            TIMINGS.with(|timings| *timings.borrow_mut() = Some(Vec::new()));
            let wall = Instant::now();
            let start_cpu = cpu();
            let report = with_reuse(reuse, || checkout.publish_changes().unwrap().unwrap());
            let total_wall = wall.elapsed().as_secs_f64() * 1000.0;
            let total_cpu = cpu().saturating_sub(start_cpu).as_secs_f64() * 1000.0;
            let timings = TIMINGS.with(|timings| timings.borrow_mut().take().unwrap());
            let mode = if reuse { "after" } else { "before" };
            let mut phases = BTreeMap::new();
            for (phase, wall, cpu) in timings {
                let total = phases.entry(phase).or_insert((0.0, 0.0));
                total.0 += wall;
                total.1 += cpu;
            }
            for (phase, (wall, cpu)) in phases {
                println!("PUBLICATION n={n} run={run} mode={mode} phase={phase} wall_ms={wall:.3} cpu_ms={cpu:.3}");
            }
            println!("PUBLICATION n={n} run={run} mode={mode} phase=total wall_ms={total_wall:.3} cpu_ms={total_cpu:.3} paths={} fragments={} retired={}", report.paths, report.fragments, report.retired_segments);
            assert_eq!(report.paths, 50);
            drop(checkout);
            std::fs::remove_dir_all(run_store).unwrap();
        }
    }
}
